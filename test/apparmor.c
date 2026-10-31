// AppArmor (kernel/src/apparmor.rs) のたしかめ: test/apparmor/test.profile (gen.sh が apparmor_parser で作った blob.h) を
// /sys/kernel/security/apparmor/.replace で読み、このプログラムの写し /tmp/aatest/t (aatest がパスでつく) に、決まりの
// 中から open、作る・消す、rename、link、chmod、truncate、flock、exec (ix px cx ux) と change_profile をためさせる。
// aatest の hat に change_hat で入る・移る・帰る (token がちがえば SIGKILL)。aanet ではソケット (network_v8)。LSM のシステムコール (lsm_get_self_attr
// など) でも読み書きする。aacap では capability (chown だけ許す)。それから、決まりのないプロセスから onexec、complain、動いているプロセスの
// プロファイルを外す (unconfined にもどる)。
// root で動かす (でなければ sudo -n でもう一度)。AppArmor のついた Linux でも同じに通ること
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/capability.h>
#include <netinet/in.h>
#include <signal.h>
#include <sys/file.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

#include "apparmor/blob.h"

#define AA "/sys/kernel/security/apparmor/"
#define D "/tmp/aatest"
static int fails;
#define CHECK(c, ...)                                                                                                  \
  do {                                                                                                                 \
    if (!(c)) {                                                                                                        \
      fails++;                                                                                                         \
      printf("FAIL " __VA_ARGS__);                                                                                     \
      printf(" (line %d)\n", __LINE__);                                                                                \
    }                                                                                                                  \
  } while (0)

static int e(int r) { return r < 0 ? errno : 0; }

static int op_open(const char *p, int fl) {
  int fd = open(p, fl, 0644);
  if (fd < 0)
    return errno;
  close(fd);
  return 0;
}

static int op_flock(const char *p) {
  int fd = open(p, O_RDONLY);
  if (fd < 0)
    return errno;
  int r = e(flock(fd, LOCK_SH));
  close(fd);
  return r;
}

// 書く口を 1 回だけ (.load .replace .remove と attr)。答えは 0 か errno
static int put(const char *path, const void *b, size_t n) {
  int fd = open(path, O_WRONLY);
  if (fd < 0)
    return errno;
  int r = write(fd, b, n) == (ssize_t)n ? 0 : errno;
  close(fd);
  return r;
}

static int puts_(const char *path, const char *s) { return put(path, s, strlen(s)); }

// ファイルの 1 行め (改行なし)。読めなければ "ERR errno"
static void line(const char *path, char *out, size_t n) {
  int fd = open(path, O_RDONLY);
  ssize_t k = fd < 0 ? -1 : read(fd, out, n - 1);
  if (k < 0) {
    snprintf(out, n, "ERR %d", errno);
  } else {
    out[k] = 0;
    out[strcspn(out, "\n")] = 0;
  }
  if (fd >= 0)
    close(fd);
}

// prog を arg で動かし、出力の 1 行め。exec できなければ "ERR errno"
static void run(const char *prog, const char *arg, char *out, size_t n) {
  int p[2];
  if (pipe(p) < 0) {
    snprintf(out, n, "ERR pipe");
    return;
  }
  fflush(stdout);
  pid_t k = fork();
  if (k == 0) {
    dup2(p[1], 1);
    close(p[0]);
    execl(prog, prog, arg, (char *)NULL);
    printf("ERR %d\n", errno);
    fflush(stdout);
    _exit(1);
  }
  close(p[1]);
  ssize_t r = read(p[0], out, n - 1);
  out[r > 0 ? r : 0] = 0;
  out[strcspn(out, "\n")] = 0;
  close(p[0]);
  int st;
  waitpid(k, &st, 0);
  // 何も言わずにシグナルで終わった (移った先で自分を写せない: SIGSEGV)
  if (!out[0] && WIFSIGNALED(st))
    snprintf(out, n, "SIG %d", WTERMSIG(st));
}

// prog を arg で動かし、出力をみな out に
static void collect(const char *prog, const char *arg, char *out, size_t n) {
  int p[2];
  out[0] = 0;
  if (pipe(p) < 0)
    return;
  fflush(stdout);
  pid_t c = fork();
  if (c == 0) {
    dup2(p[1], 1);
    close(p[0]);
    execl(prog, prog, arg, (char *)NULL);
    _exit(127);
  }
  close(p[1]);
  size_t got = 0;
  ssize_t k;
  while ((k = read(p[0], out + got, n - 1 - got)) > 0)
    got += k;
  out[got] = 0;
  close(p[0]);
  waitpid(c, NULL, 0);
}

// capget の effective と permitted の下の 32 ビット
static void caps(unsigned *eff, unsigned *perm) {
  struct __user_cap_header_struct h = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct d[2] = {0};
  syscall(SYS_capget, &h, d);
  *eff = d[0].effective;
  *perm = d[0].permitted;
}

// aacap (argv[1] が cap) か aacomplain (capc) で: 1 行に「名前 答え」。cap2 は uid 1 の 0600
static int capchild(int complain) {
  if (puts_("/proc/self/attr/current", complain ? "changeprofile aacomplain" : "changeprofile aacap"))
    return 2;
  const char *k = complain ? "capc" : "cap";
  unsigned eff, perm;
  caps(&eff, &perm);
  printf("%s-eff %x\n%s-perm %x\n", k, eff, k, perm);
  printf("%s-read-other %d\n", k, op_open(D "/cap2", O_RDONLY));
  printf("%s-write-other %d\n", k, op_open(D "/cap2", O_WRONLY));
  printf("%s-chmod-other %d\n", k, e(chmod(D "/cap2", 0600)));
  printf("%s-chown %d\n", k, e(chown(D "/cap", 1, 1)));
  printf("%s-chown-back %d\n", k, e(chown(D "/cap", 0, 0)));
  printf("%s-mknod %d\n", k, e(mknod(D "/dev0", S_IFCHR | 0600, makedev(1, 3))));
  unlink(D "/dev0");
  printf("%s-setuid %d\n", k, e(setuid(1)));
  fflush(stdout);
  return 0;
}

// LSM のシステムコール (Linux 6.8 から。aarch64 も同じ番号) と struct lsm_ctx
#define NR_LSM_GET 459
#define NR_LSM_SET 460
#define NR_LSM_LIST 461
struct lctx {
  unsigned long long id, flags, len, ctx_len;
  char ctx[200];
};

// lsm_get_self_attr(attr) の答え: "数 id ctx_len 中身" か "ERR errno"
static void lsm_get(const char *name, int attr) {
  struct lctx c = {0};
  unsigned size = sizeof c;
  long r = syscall(NR_LSM_GET, attr, &c, &size, 0);
  if (r < 0)
    printf("%s ERR %d\n", name, errno);
  else
    printf("%s %ld %llu %llu %.*s\n", name, r, c.id, c.ctx_len, (int)c.ctx_len, c.ctx);
}

// 大きさだけを問う (ctx なし)、足りない大きさ、ひとつの LSM だけ (flags の LSM_FLAG_SINGLE)
static void lsm_sizes(void) {
  unsigned size = 4096;
  long r = syscall(NR_LSM_GET, 100, NULL, &size, 0);
  printf("lsm-null %ld %u\n", r, size);
  struct lctx c = {0};
  size = 8;
  r = syscall(NR_LSM_GET, 100, &c, &size, 0);
  printf("lsm-small %ld %d %u\n", r, r < 0 ? errno : 0, size);
  c.id = 100; // capability は答えない
  size = sizeof c;
  r = syscall(NR_LSM_GET, 100, &c, &size, 1);
  printf("lsm-single-cap %ld %d\n", r, r < 0 ? errno : 0);
  memset(&c, 0, sizeof c);
  c.id = 104;
  size = sizeof c;
  r = syscall(NR_LSM_GET, 100, &c, &size, 1);
  printf("lsm-single-aa %ld %.*s\n", r, (int)c.ctx_len, c.ctx);
}

// lsm_set_self_attr で AppArmor に s を書く (attr/current に書くのと同じ)
static int lsm_set(int attr, const char *s) {
  struct lctx c = {0};
  c.id = 104;
  c.ctx_len = strlen(s);
  c.len = (32 + c.ctx_len + 7) / 8 * 8;
  memcpy(c.ctx, s, c.ctx_len);
  return syscall(NR_LSM_SET, attr, &c, (unsigned)c.len, 0) < 0 ? errno : 0;
}

// /proc/self/attr/current に n バイト書く (changehat は hat を \0 で区切って続けられる)
static int hatw(const char *s, size_t n) { return put("/proc/self/attr/current", s, n); }
#define HAT(s) hatw(s, strlen(s))

// 子を待って、シグナルで終わったら "SIG n"、でなければ "EXIT n"
static void ended(const char *name, pid_t k) {
  int st;
  waitpid(k, &st, 0);
  if (WIFSIGNALED(st))
    printf("%s SIG %d\n", name, WTERMSIG(st));
  else
    printf("%s EXIT %d\n", name, WEXITSTATUS(st));
}

// aatest の中で change_hat (どれも fork した子で。入ったり殺されたりするので)
static void hats(void) {
#define L(name, path)                                                                                                  \
  do {                                                                                                                 \
    char b_[128];                                                                                                      \
    line(path, b_, sizeof b_);                                                                                         \
    printf("%s %s\n", name, b_);                                                                                       \
  } while (0)
  pid_t k;
  fflush(stdout);
  if ((k = fork()) == 0) {
    printf("hat-in %d\n", HAT("changehat 1234^hat1"));
    L("hat-cur", "/proc/self/attr/current");
    L("hat-prev", "/proc/self/attr/prev");
    printf("hat-open-w %d\nhat-open-r %d\n", op_open(D "/w", O_RDONLY), op_open(D "/r", O_RDONLY));
    printf("hat-sibling %d\n", HAT("changehat 1234^hat2"));
    L("hat-cur2", "/proc/self/attr/current");
    lsm_get("lsm-hatprev", 104);
    printf("hat-out %d\n", HAT("changehat 1234^"));
    L("hat-back", "/proc/self/attr/current");
    L("hat-prev2", "/proc/self/attr/prev");
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
  // 入れないもの、変えないもの
  if ((k = fork()) == 0) {
    printf("hat-none %d\n", HAT("changehat 1234^nohat"));
    printf("hat-nothat %d\n", HAT("changehat 1234^sub"));
    printf("hat-zero %d\n", HAT("changehat 0^"));
    printf("hat-junk %d\n", HAT("changehat xyz^hat1"));
    printf("hat-noprev %d\n", HAT("changehat 55^"));
    printf("hat-perm %d\n", HAT("permhat 1234^hat1"));
    L("hat-perm-cur", "/proc/self/attr/current");
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
  // いくつも (libapparmor の aa_change_hatv と同じ形: 名前ごとに \0、終わりにもう 1 つ)
  if ((k = fork()) == 0) {
    static const char v[] = "changehat 00000000000004d2^nohat\0hat1\0";
    printf("hat-vec %d\n", hatw(v, sizeof v));
    L("hat-vec-cur", "/proc/self/attr/current");
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
  // token がちがえば SIGKILL (帰るときも、兄弟へ移るときも)
  if ((k = fork()) == 0) {
    HAT("changehat 1234^hat1");
    HAT("changehat 99^");
    _exit(0);
  }
  ended("hat-badtoken", k);
  if ((k = fork()) == 0) {
    HAT("changehat 1234^hat1");
    HAT("changehat 99^hat2");
    _exit(0);
  }
  ended("hat-badtoken2", k);
  // hat のないプロファイルから
  if ((k = fork()) == 0) {
    puts_("/proc/self/attr/current", "changeprofile aachild");
    printf("hat-nohats %d\n", HAT("changehat 1234^x"));
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
  // LSM のシステムコール
  lsm_get("lsm-cur", 100);
  lsm_get("lsm-exec", 101);
  lsm_get("lsm-prev", 104);
  lsm_sizes();
  if ((k = fork()) == 0) {
    printf("lsm-set %d\n", lsm_set(100, "changeprofile aachild"));
    L("lsm-set-cur", "/proc/self/attr/current");
    printf("lsm-set-hat %d\n", lsm_set(102, "changehat 1^x"));
    fflush(stdout);
    _exit(0);
  }
  waitpid(k, NULL, 0);
#undef L
}

// 抽象名の sockaddr_un (長さは名前まで)
static socklen_t abstract(struct sockaddr_un *a, const char *name) {
  memset(a, 0, sizeof *a);
  a->sun_family = AF_UNIX;
  memcpy(a->sun_path + 1, name, strlen(name));
  return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
}

// aaunix の中で (argv[1] が unix): AF_UNIX の細かい決まり (network_v9)。1 行に「名前 答え」
static int unixchild(void) {
  if (puts_("/proc/self/attr/current", "changeprofile aaunix"))
    return 2;
#define U(name, v) printf("%s %d\n", name, v)
  struct sockaddr_un a, b;
  struct sockaddr_un n;
  socklen_t al = abstract(&a, "aaunix-s"), bl;
  int one = 1, t;
  socklen_t tl = sizeof t;
  int l = socket(AF_UNIX, SOCK_STREAM, 0);
  U("uni-create", l < 0 ? errno : 0);
  U("uni-bind", e(bind(l, (struct sockaddr *)&a, al)));
  int x = socket(AF_UNIX, SOCK_STREAM, 0);
  bl = abstract(&b, "aaunix-x");
  U("uni-bind-other", e(bind(x, (struct sockaddr *)&b, bl)));
  int w = socket(AF_UNIX, SOCK_STREAM, 0);
  socklen_t wl = abstract(&n, "aaunix-w");
  U("uni-bind-w", e(bind(w, (struct sockaddr *)&n, wl)));
  U("uni-listen", e(listen(l, 4)));
  socklen_t nl = sizeof n;
  U("uni-getname", e(getsockname(l, (struct sockaddr *)&n, &nl)));
  U("uni-setopt", e(setsockopt(l, SOL_SOCKET, SO_PASSCRED, &one, sizeof one)));
  U("uni-getopt", e(getsockopt(l, SOL_SOCKET, SO_TYPE, &t, &tl)));
  int c = socket(AF_UNIX, SOCK_STREAM, 0);
  U("uni-connect", e(connect(c, (struct sockaddr *)&a, al)));
  int k = accept(l, NULL, NULL);
  U("uni-accept", k < 0 ? errno : 0);
  U("uni-connect-other", e(connect(x, (struct sockaddr *)&b, bl)));
  U("uni-listen-other", e(listen(x, 4)));
  U("uni-getname-other", e(getsockname(x, (struct sockaddr *)&n, &nl)));
  nl = sizeof n;
  U("uni-getname-anon", e(getsockname(c, (struct sockaddr *)&n, &nl)));
  // 決まりのない外のもの: @aaunix-y で listen、@aaunix-z の dgram
  int y = socket(AF_UNIX, SOCK_STREAM, 0);
  bl = abstract(&b, "aaunix-y");
  U("uni-connect-y", e(connect(y, (struct sockaddr *)&b, bl)));
  int z = socket(AF_UNIX, SOCK_DGRAM, 0);
  bl = abstract(&b, "aaunix-z");
  U("uni-dgram-send-z", e(sendto(z, "x", 1, 0, (struct sockaddr *)&b, bl)));
  // dgram
  int d = socket(AF_UNIX, SOCK_DGRAM, 0);
  bl = abstract(&b, "aaunix-d");
  U("uni-dgram-bind", e(bind(d, (struct sockaddr *)&b, bl)));
  int s = socket(AF_UNIX, SOCK_DGRAM, 0);
  U("uni-dgram-send", e(sendto(s, "x", 1, 0, (struct sockaddr *)&b, bl)));
  char buf[8];
  U("uni-dgram-recv", e(recv(d, buf, sizeof buf, MSG_DONTWAIT)));
  U("uni-dgram-send-self", e(sendto(d, "x", 1, 0, (struct sockaddr *)&b, bl)));
  int sv[2];
  U("uni-dgrampair", e(socketpair(AF_UNIX, SOCK_DGRAM, 0, sv)));
  U("uni-seqpacket", socket(AF_UNIX, SOCK_SEQPACKET, 0) < 0 ? errno : 0);
  // パスのソケット: 外で bind した /tmp/aatest/sock へ (ファイルの決まりの rw)
  struct sockaddr_un p = {.sun_family = AF_UNIX};
  strcpy(p.sun_path, D "/sock");
  int f = socket(AF_UNIX, SOCK_STREAM, 0);
  U("uni-fs-connect", e(connect(f, (struct sockaddr *)&p, sizeof p)));
  // ファイルの決まりのない /tmp/aatest/sock2 へは (相手は決まりのないもの)
  strcpy(p.sun_path, D "/sock2");
  int f2 = socket(AF_UNIX, SOCK_STREAM, 0);
  U("uni-fs-connect-other", e(connect(f2, (struct sockaddr *)&p, sizeof p)));
  fflush(stdout);
#undef U
  return 0;
}

// aamnt の中で (argv[1] が mnt): mount の決まり。m2 には外で tmpfs をかぶせてある
static int mntchild(void) {
  if (puts_("/proc/self/attr/current", "changeprofile aamnt"))
    return 2;
  printf("mnt-tmpfs %d\n", e(mount("none", D "/m1", "tmpfs", 0, NULL)));
  printf("mnt-tmpfs-other %d\n", e(mount("none", D "/d", "tmpfs", 0, NULL)));
  printf("mnt-proc %d\n", e(mount("proc", D "/m3", "proc", 0, NULL)));
  printf("mnt-nosuid %d\n", e(mount("none", D "/m3", "tmpfs", MS_NOSUID, NULL)));
  printf("mnt-nosuid-umount %d\n", e(umount(D "/m3")));
  printf("mnt-rw %d\n", e(mount("none", D "/m3", "tmpfs", 0, NULL)));
  printf("mnt-bind %d\n", e(mount(D "/d", D "/m3", NULL, MS_BIND, NULL)));
  printf("mnt-nofs %d\n", e(mount("none", D "/m3", "nosuchfs", 0, NULL)));
  printf("mnt-umount %d\n", e(umount(D "/m1")));
  printf("mnt-umount-other %d\n", e(umount(D "/m2")));
  printf("mnt-umount-notmounted %d\n", e(umount(D "/m1")));
  fflush(stdout);
  return 0;
}

// aanet の中で (argv[1] が net): ソケットの決まり。1 行に「名前 答え」
static int netchild(void) {
  if (puts_("/proc/self/attr/current", "changeprofile aanet"))
    return 2;
#define N(name, v) printf("%s %d\n", name, v)
  struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  socklen_t al = sizeof a;
  int one = 1, t;
  // inet の stream: みな
  int l = socket(AF_INET, SOCK_STREAM, 0);
  N("net-tcp-create", l < 0 ? errno : 0);
  N("net-tcp-bind", e(bind(l, (struct sockaddr *)&a, sizeof a)));
  N("net-tcp-listen", e(listen(l, 4)));
  N("net-tcp-name", e(getsockname(l, (struct sockaddr *)&a, &al)));
  int c = socket(AF_INET, SOCK_STREAM, 0);
  N("net-tcp-connect", e(connect(c, (struct sockaddr *)&a, sizeof a)));
  int k = accept(l, NULL, NULL);
  N("net-tcp-accept", k < 0 ? errno : 0);
  N("net-tcp-send", e(send(c, "x", 1, 0) == 1 ? 0 : -1));
  char b[8];
  N("net-tcp-recv", e(recv(k, b, sizeof b, 0) == 1 ? 0 : -1));
  // inet の dgram: 作る、bind、名前だけ
  int u = socket(AF_INET, SOCK_DGRAM, 0);
  N("net-udp-create", u < 0 ? errno : 0);
  struct sockaddr_in ua = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  al = sizeof ua;
  N("net-udp-bind", e(bind(u, (struct sockaddr *)&ua, sizeof ua)));
  N("net-udp-name", e(getsockname(u, (struct sockaddr *)&ua, &al)));
  N("net-udp-send", e(sendto(u, "x", 1, 0, (struct sockaddr *)&ua, sizeof ua)));
  N("net-udp-recv", e(recv(u, b, sizeof b, MSG_DONTWAIT)));
  N("net-udp-setopt", e(setsockopt(u, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one)));
  al = sizeof t;
  N("net-udp-getopt", e(getsockopt(u, SOL_SOCKET, SO_TYPE, &t, &al)));
  N("net-udp-shutdown", e(shutdown(u, SHUT_RDWR)));
  N("net-udp-peername", e(getpeername(u, (struct sockaddr *)&ua, &al)));
  // 決まりのない family と型
  int x = socket(AF_INET6, SOCK_STREAM, 0);
  N("net-inet6", x < 0 ? errno : 0);
  x = socket(AF_NETLINK, SOCK_RAW, 0);
  N("net-netlink", x < 0 ? errno : 0);
  x = socket(AF_UNIX, SOCK_STREAM, 0);
  N("net-unix-stream", x < 0 ? errno : 0);
  int sv[2] = {-1, -1};
  N("net-unix-streampair", e(socketpair(AF_UNIX, SOCK_STREAM, 0, sv)));
  // unix の dgram: みな (送るのは自分 (SEND) と相手のソケットのラベル (RECEIVE) の両方で見る)
  N("net-unix-dgrampair", e(socketpair(AF_UNIX, SOCK_DGRAM, 0, sv)));
  N("net-unix-dgram-send", e(write(sv[0], "x", 1) == 1 ? 0 : -1));
  N("net-unix-dgram-recv", e(read(sv[1], b, sizeof b) == 1 ? 0 : -1));
  fflush(stdout);
#undef N
  return 0;
}

// 決まりのあるプロセス (profile) が抽象名 name で作った unix の口へ、決まりのないこちらから (type が SOCK_STREAM なら
// connect、SOCK_DGRAM なら sendto)。答えは 0 か errno。相手の側の決まりでも止まる
static int peer_test(const char *profile, int type, const char *name) {
  int go[2], ready[2];
  if (pipe(go) || pipe(ready))
    return -1;
  struct sockaddr_un a = {.sun_family = AF_UNIX};
  memcpy(a.sun_path + 1, name, strlen(name));
  socklen_t al = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
  fflush(stdout);
  pid_t k = fork();
  if (k == 0) {
    char x = 'f';
    if (puts_("/proc/self/attr/current", profile) == 0) {
      int s = socket(AF_UNIX, type, 0);
      if (s >= 0 && bind(s, (struct sockaddr *)&a, al) == 0 && (type != SOCK_STREAM || listen(s, 4) == 0))
        x = 'r';
    }
    if (write(ready[1], &x, 1) != 1 || read(go[0], &x, 1) != 1)
      _exit(1);
    _exit(0);
  }
  char x = 0;
  int r = -1;
  if (read(ready[0], &x, 1) == 1 && x == 'r') {
    int c = socket(AF_UNIX, type, 0);
    if (type == SOCK_STREAM)
      r = e(connect(c, (struct sockaddr *)&a, al));
    else
      r = e(sendto(c, "x", 1, 0, (struct sockaddr *)&a, al));
    close(c);
  }
  if (write(go[1], "g", 1) != 1)
    r = -1;
  waitpid(k, NULL, 0);
  close(go[0]);
  close(go[1]);
  close(ready[0]);
  close(ready[1]);
  return r;
}

// path を開いて読む: 0 か errno
static int op_read(const char *path) {
  char b[64];
  int fd = open(path, O_RDONLY);
  if (fd < 0)
    return errno;
  int r = read(fd, b, sizeof b) < 0 ? errno : 0;
  close(fd);
  return r;
}

// readlink: 0 か errno
static int op_readlink(const char *path) {
  char b[256];
  return readlink(path, b, sizeof b) < 0 ? errno : 0;
}

// シグナルを待つだけの相手 (profile があればそれで)。usr1 は無視する。go から読めれば終わる
static pid_t target(const char *profile, int go) {
  fflush(stdout);
  pid_t k = fork();
  if (k == 0) {
    char x;
    signal(SIGUSR1, SIG_IGN);
    if (profile && puts_("/proc/self/attr/current", profile))
      _exit(2);
    if (read(go, &x, 1) < 0)
      _exit(1);
    _exit(0);
  }
  return k;
}

// シグナルと ptrace: t1 は決まりなし、t2 は aanet (シグナルの決まりがない: 受けられない。ptrace でも見られない)
static void sigptrace(char *out, size_t n) {
  int go[2], o[2];
  out[0] = 0;
  if (pipe(go) || pipe(o))
    return;
  pid_t t1 = target(NULL, go[0]), t2 = target("changeprofile aanet", go[0]);
  usleep(200000);
  char p[64];
  FILE *f = fmemopen(out, n, "w");
  // 決まりのないこちらから: 送る側は見ないが、受ける側 (aanet) の決まりで止まる。ptrace は追う側が決まりなしなら見ない
  fprintf(f, "sig-u-t2 %d\n", e(kill(t2, SIGUSR1)));
  fprintf(f, "sig-u-t2-exists %d\n", e(kill(t2, 0)));
  snprintf(p, sizeof p, "/proc/%d/environ", t2);
  fprintf(f, "pt-u-t2-environ %d\n", op_read(p));
  fflush(f);
  pid_t c = fork();
  if (c == 0) {
    char b[1024] = "";
    FILE *g = fmemopen(b, sizeof b, "w");
    signal(SIGUSR1, SIG_IGN);
    if (puts_("/proc/self/attr/current", "changeprofile aasig"))
      _exit(2);
    fprintf(g, "sig-t1-usr1 %d\n", e(kill(t1, SIGUSR1)));
    fprintf(g, "sig-t1-term %d\n", e(kill(t1, SIGTERM)));
    fprintf(g, "sig-t1-exists %d\n", e(kill(t1, 0)));
    fprintf(g, "sig-self-usr1 %d\n", e(kill(getpid(), SIGUSR1)));
    fprintf(g, "sig-t2-usr1 %d\n", e(kill(t2, SIGUSR1)));
    snprintf(p, sizeof p, "/proc/%d/environ", t1);
    fprintf(g, "pt-t1-environ %d\n", op_read(p));
    snprintf(p, sizeof p, "/proc/%d/exe", t1);
    fprintf(g, "pt-t1-exe %d\n", op_readlink(p));
    snprintf(p, sizeof p, "/proc/%d/environ", t2);
    fprintf(g, "pt-t2-environ %d\n", op_read(p));
    snprintf(p, sizeof p, "/proc/%d/exe", t2);
    fprintf(g, "pt-t2-exe %d\n", op_readlink(p));
    fprintf(g, "pt-self-environ %d\n", op_read("/proc/self/environ"));
    fclose(g);
    if (write(o[1], b, strlen(b)) < 0)
      _exit(1);
    _exit(0);
  }
  close(o[1]);
  char b[1024];
  ssize_t k = read(o[0], b, sizeof b - 1);
  b[k > 0 ? k : 0] = 0;
  fputs(b, f);
  fclose(f);
  waitpid(c, NULL, 0);
  if (write(go[1], "gg", 2) != 2)
    kill(t1, SIGKILL);
  waitpid(t1, NULL, 0);
  waitpid(t2, NULL, 0);
  close(go[0]);
  close(go[1]);
  close(o[0]);
}

// aatest の中で (argv[1] が child): 1 行に「名前 答え」
static int child(void) {
#define T(name, v) printf("%s %d\n", name, v)
  struct stat st;
  T("open-r", op_open(D "/r", O_RDONLY));
  T("open-r-w", op_open(D "/r", O_WRONLY));
  T("open-w", op_open(D "/w", O_WRONLY));
  T("open-w-r", op_open(D "/w", O_RDONLY));
  T("open-rw", op_open(D "/rw", O_RDWR));
  T("open-a", op_open(D "/a", O_WRONLY | O_APPEND));
  T("open-a-w", op_open(D "/a", O_WRONLY));
  T("open-deny", op_open(D "/deny", O_RDONLY));
  T("open-x", op_open(D "/x", O_RDONLY));
  T("open-missing", op_open(D "/missing", O_RDONLY));
  T("stat-x", e(stat(D "/x", &st)));
  T("create-d", op_open(D "/d/f", O_WRONLY | O_CREAT));
  T("create-top", op_open(D "/new", O_WRONLY | O_CREAT));
  T("mkdir-d", e(mkdir(D "/d/sub", 0755)));
  T("rmdir-d", e(rmdir(D "/d/sub")));
  T("rename-in-d", e(rename(D "/d/f", D "/d/g")));
  T("rename-out", e(rename(D "/d/g", D "/out")));
  T("unlink-d", e(unlink(D "/d/g")));
  T("unlink-r", e(unlink(D "/r")));
  T("opendir-d", op_open(D "/d", O_RDONLY | O_DIRECTORY));
  T("opendir-top", op_open(D, O_RDONLY | O_DIRECTORY));
  T("open-own", op_open(D "/own", O_RDONLY));
  T("flock-k", op_flock(D "/k"));
  T("flock-r", op_flock(D "/r"));
  T("link-ok", e(link(D "/r", D "/ln")));
  T("link-bad", e(link(D "/rw", D "/ln2")));
  T("chmod-rw", e(chmod(D "/rw", 0644)));
  T("chmod-r", e(chmod(D "/r", 0644)));
  T("truncate-w", e(truncate(D "/w", 0)));
  T("truncate-r", e(truncate(D "/r", 0)));
  char b[128];
  line("/proc/self/attr/current", b, sizeof b);
  printf("self %s\n", b);
  const char *ex[] = {"ix", "px", "cx", "ux", "nox", "pnomap"};
  for (int i = 0; i < 6; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", ex[i]);
    run(p, "attr", b, sizeof b);
    printf("exec-%s %s\n", ex[i], b);
  }
  hats();
  T("change-complain", puts_("/proc/self/attr/current", "changeprofile aacomplain"));
  T("change-child", puts_("/proc/self/attr/current", "changeprofile aachild"));
  line("/proc/self/attr/current", b, sizeof b);
  printf("changed %s\n", b);
  fflush(stdout);
  return 0;
}

// 答え: 子の出力に want の行がみなあるか
static const char *want[] = {
    "open-r 0", "open-r-w 13", "open-w 0", "open-w-r 13", "open-rw 0", "open-a 0", "open-a-w 13", "open-deny 13",
    "open-x 13", "open-missing 2", "stat-x 0", "create-d 0", "create-top 13", "mkdir-d 0", "rmdir-d 0",
    "rename-in-d 0", "rename-out 13", "unlink-d 0", "unlink-r 13", "opendir-d 0", "opendir-top 13", "open-own 0",
    "flock-k 0", "flock-r 13", "link-ok 0", "link-bad 13", "chmod-rw 0", "chmod-r 13", "truncate-w 0",
    "truncate-r 13", "self aatest (enforce)", "exec-ix aatest (enforce)", "exec-px aachild (enforce)",
    "exec-cx aatest//sub (enforce)", "exec-ux unconfined", "exec-nox ERR 13", "exec-pnomap SIG 11", "change-complain 13", "change-child 0",
    "changed aachild (enforce)", "hat-in 0", "hat-cur aatest//hat1 (enforce)", "hat-prev aatest (enforce)",
    "hat-open-w 0", "hat-open-r 13", "hat-sibling 0", "hat-cur2 aatest//hat2 (enforce)", "hat-out 0",
    "hat-back aatest (enforce)", "hat-prev2 ERR 22", "hat-none 2", "hat-nothat 1", "hat-zero 22", "hat-junk 22",
    "hat-noprev 0", "hat-perm 0", "hat-perm-cur aatest (enforce)", "hat-vec 0", "hat-vec-cur aatest//hat1 (enforce)",
    "hat-badtoken SIG 9", "hat-badtoken2 SIG 9", "hat-nohats 10", "lsm-hatprev 1 104 16 aatest (enforce)",
    "lsm-cur 1 104 16 aatest (enforce)", "lsm-exec ERR 2", "lsm-prev ERR 2", "lsm-null 1 48", "lsm-small -1 7 48",
    "lsm-single-cap -1 95", "lsm-single-aa 1 aatest (enforce)", "lsm-set 0", "lsm-set-cur aachild (enforce)",
    "lsm-set-hat 95",
};

// シグナルと ptrace (aasig: usr1 だけ送れる、unconfined だけ見られる。aanet: 受けられない、見られない)
static const char *want_sig[] = {
    "sig-u-t2 13", "sig-u-t2-exists 13", "pt-u-t2-environ 0", "sig-t1-usr1 0", "sig-t1-term 13", "sig-t1-exists 13",
    "sig-self-usr1 0", "sig-t2-usr1 13", "pt-t1-environ 0", "pt-t1-exe 0", "pt-t2-environ 13", "pt-t2-exe 13",
    "pt-self-environ 0",
};

// aamnt: m1 の tmpfs と m3 の nosuid の tmpfs だけ。外せるのは m1 だけ
static const char *want_mnt[] = {
    "mnt-tmpfs 0", "mnt-tmpfs-other 13", "mnt-proc 13", "mnt-nosuid 0", "mnt-nosuid-umount 13", "mnt-rw 13", "mnt-bind 13",
    "mnt-nofs 19", "mnt-umount 0", "mnt-umount-other 13", "mnt-umount-notmounted 22",
};

// aaunix: 番地 (@aaunix-s と @aaunix-d)、cmd (setopt / getopt はない)、相手の番地とラベル、deny (@aaunix-x)、パスのソケット
// (ファイルの決まり)。Linux 7.3.1 (apparmor_parser 5.0 で作ったもの) と同じ
static const char *want_unix[] = {
    "uni-create 0", "uni-bind 0", "uni-bind-other 13", "uni-bind-w 0", "uni-listen 0", "uni-getname 0",
    "uni-setopt 13", "uni-getopt 13", "uni-connect 0", "uni-accept 0", "uni-connect-other 111",
    "uni-listen-other 13", "uni-getname-other 13", "uni-getname-anon 13", "uni-connect-y 13",
    "uni-dgram-send-z 0", "uni-dgram-bind 0", "uni-dgram-send 0", "uni-dgram-recv 0", "uni-dgram-send-self 0",
    "uni-dgrampair 0", "uni-seqpacket 13", "uni-fs-connect 0", "uni-fs-connect-other 13",
};

// aanet: 決まりにあるソケットだけ (ほかの family、型、許しは EACCES)
static const char *want_net[] = {
    "net-tcp-create 0", "net-tcp-bind 0", "net-tcp-listen 0", "net-tcp-name 0", "net-tcp-connect 0", "net-tcp-accept 0",
    "net-tcp-send 0", "net-tcp-recv 0", "net-udp-create 0", "net-udp-bind 0", "net-udp-name 0", "net-udp-send 13",
    "net-udp-recv 13", "net-udp-setopt 13", "net-udp-getopt 13", "net-udp-shutdown 13", "net-udp-peername 107",
    "net-inet6 13", "net-netlink 13", "net-unix-stream 13", "net-unix-streampair 13", "net-unix-dgrampair 0",
    "net-unix-dgram-send 0", "net-unix-dgram-recv 0",
};

// aacap: chown のほかの root の力はない (capget にも chown だけ)。complain なら記録して通す
static const char *want_cap[] = {
    "cap-eff 1", "cap-perm 1", "cap-read-other 13", "cap-write-other 13", "cap-chmod-other 1", "cap-chown 0",
    "cap-chown-back 0", "cap-mknod 1", "cap-setuid 1", "capc-eff ffffffff", "capc-perm ffffffff",
    "capc-read-other 0", "capc-write-other 0", "capc-chmod-other 0", "capc-chown 0", "capc-chown-back 0",
    "capc-mknod 0", "capc-setuid 0",
};

// 作るものと、このプログラムの写し (t と、exec の行き先)。Linux の VM (test/linux-vm.sh の rootexec) には
// シェルも cp もないので、自分で
static const char *files[] = {"r", "w", "rw", "a", "deny", "x", "own", "k", "cap", "cap2"};
static const char *bins[] = {"t", "ix", "px", "cx", "ux", "nox", "pnomap"};
// テストでできるかもしれないもの
static const char *made[] = {"ln", "ln2", "new", "out", "d/f", "d/g", "dev0", "sock", "sock2"};

static int copy(const char *from, const char *to) {
  int in = open(from, O_RDONLY), out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
  char b[65536];
  ssize_t n;
  int ok = in >= 0 && out >= 0;
  while (ok && (n = read(in, b, sizeof b)) > 0)
    ok = write(out, b, n) == n;
  if (in >= 0)
    close(in);
  if (out >= 0)
    close(out);
  return ok;
}

static void setup(const char *self) {
  mkdir(D, 0755);
  mkdir(D "/d", 0755);
  mkdir(D "/m1", 0755);
  mkdir(D "/m2", 0755);
  mkdir(D "/m3", 0755);
  for (size_t i = 0; i < sizeof files / sizeof *files; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", files[i]);
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) {
      CHECK(write(fd, "x\n", 2) == 2, "write %s", p);
      close(fd);
    }
  }
  for (size_t i = 0; i < sizeof bins / sizeof *bins; i++) {
    char p[64];
    snprintf(p, sizeof p, D "/%s", bins[i]);
    CHECK(copy(self, p), "copy %s", bins[i]);
  }
}

static void cleanup(void) {
  puts_(AA ".remove", "aatest");
  puts_(AA ".remove", "aachild");
  puts_(AA ".remove", "aacomplain");
  puts_(AA ".remove", "aanomap");
  puts_(AA ".remove", "aacap");
  puts_(AA ".remove", "aanet");
  puts_(AA ".remove", "aanetl");
  puts_(AA ".remove", "aanetd");
  puts_(AA ".remove", "aasig");
  puts_(AA ".remove", "aamnt");
  puts_(AA ".remove", "aaunix");
  const char **lists[] = {files, bins, made};
  size_t ns[] = {sizeof files / sizeof *files, sizeof bins / sizeof *bins, sizeof made / sizeof *made};
  for (int l = 0; l < 3; l++)
    for (size_t i = 0; i < ns[l]; i++) {
      char p[64];
      snprintf(p, sizeof p, D "/%s", lists[l][i]);
      unlink(p);
    }
  rmdir(D "/d/sub");
  for (int i = 0; i < 2; i++) {
    umount(D "/m1");
    umount(D "/m2");
    umount(D "/m3");
  }
  rmdir(D "/m1");
  rmdir(D "/m2");
  rmdir(D "/m3");
  rmdir(D "/d");
  rmdir(D);
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "child"))
    return child();
  if (argc > 1 && !strcmp(argv[1], "net"))
    return netchild();
  if (argc > 1 && !strcmp(argv[1], "mnt"))
    return mntchild();
  if (argc > 1 && !strcmp(argv[1], "unix"))
    return unixchild();
  if (argc > 1 && (!strcmp(argv[1], "cap") || !strcmp(argv[1], "capc")))
    return capchild(argv[1][3] == 'c');
  if (argc > 1 && !strcmp(argv[1], "attr")) {
    char b[128];
    line("/proc/self/attr/current", b, sizeof b);
    printf("%s\n", b);
    return 0;
  }
  char self[256];
  ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
  self[n > 0 ? n : 0] = 0;
  if (geteuid() != 0) {
    execl("/usr/bin/sudo", "sudo", "-n", self, (char *)NULL);
    printf("apparmor: needs root (sudo -n failed)\n");
    return 1;
  }
  cleanup();
  setup(self);

  // 読みこむ。同じものの .load は EEXIST、.replace はよい
  CHECK(put(AA ".replace", aa_blob, sizeof aa_blob) == 0, ".replace");
  CHECK(put(AA ".load", aa_blob, sizeof aa_blob) == EEXIST, ".load again is EEXIST");
  CHECK(put(AA ".replace", aa_blob, sizeof aa_blob) == 0, ".replace again");
  CHECK(put(AA ".replace", "nonsense", 8) == EPROTONOSUPPORT, ".replace of junk");
  char prof[4096] = "";
  int fd = open(AA "profiles", O_RDONLY);
  ssize_t k = fd < 0 ? -1 : read(fd, prof, sizeof prof - 1);
  prof[k > 0 ? k : 0] = 0;
  if (fd >= 0)
    close(fd);
  CHECK(strstr(prof, "aatest (enforce)\n") && strstr(prof, "aatest//sub (enforce)\n") && strstr(prof, "aachild (enforce)\n") && strstr(prof, "aacomplain (complain)\n"), "profiles: %s", prof);

  // .access: aatest で /tmp/aatest/r は読める (0x4)、/tmp/aatest/w は読めない
  fd = open(AA ".access", O_RDWR);
  char q[] = "label\0aatest\0\x02" D "/r", ans[256] = "";
  CHECK(fd >= 0 && write(fd, q, sizeof q - 1) == sizeof q - 1 && read(fd, ans, sizeof ans - 1) > 0, ".access");
  unsigned allow = 0;
  sscanf(ans, "allow 0x%x", &allow);
  CHECK(allow & 4, ".access allow 0x%x", allow);
  if (fd >= 0)
    close(fd);

  // aatest の中で
  char out[8192] = "";
  collect(D "/t", "child", out, sizeof out);
  int bad = fails;
  for (size_t i = 0; i < sizeof want / sizeof *want; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want[i]);
    CHECK(strstr(out, l), "want \"%s\"", want[i]);
  }
  if (fails != bad)
    printf("child said:\n%s", out);

  // capability: aacap と aacomplain の中で (cap2 は uid 1 の 0600)
  CHECK(chown(D "/cap2", 1, 1) == 0 && chmod(D "/cap2", 0600) == 0, "cap2");
  char cout[2048];
  collect(self, "cap", out, sizeof out);
  collect(self, "capc", cout, sizeof cout);
  strncat(out, cout, sizeof out - strlen(out) - 1);
  bad = fails;
  for (size_t i = 0; i < sizeof want_cap / sizeof *want_cap; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want_cap[i]);
    CHECK(strstr(out, l), "want \"%s\"", want_cap[i]);
  }
  if (fails != bad)
    printf("cap said:\n%s", out);

  // 決まりのないプロセスから: onexec (次の exec で aachild)
  char b[128];
  int st;
  pid_t c = fork();
  if (c == 0) {
    if (puts_("/proc/self/attr/exec", "exec aachild"))
      _exit(2);
    line("/proc/self/attr/exec", b, sizeof b);
    _exit(strcmp(b, "aachild (enforce)") ? 3 : 0);
  }
  waitpid(c, &st, 0);
  CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "onexec set (%d)", WEXITSTATUS(st));
  int q2[2];
  pipe(q2);
  fflush(stdout);
  c = fork();
  if (c == 0) {
    dup2(q2[1], 1);
    puts_("/proc/self/attr/exec", "exec aachild");
    execl(D "/ix", D "/ix", "attr", (char *)NULL);
    _exit(127);
  }
  close(q2[1]);
  k = read(q2[0], b, sizeof b - 1);
  b[k > 0 ? k : 0] = 0;
  b[strcspn(b, "\n")] = 0;
  close(q2[0]);
  waitpid(c, NULL, 0);
  CHECK(!strcmp(b, "aachild (enforce)"), "onexec gave \"%s\"", b);

  // ネットワーク: aanet の中で。それから相手の側の決まり (listen しているもの、bind した dgram の口のラベル)
  collect(self, "net", out, sizeof out);
  bad = fails;
  for (size_t i = 0; i < sizeof want_net / sizeof *want_net; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want_net[i]);
    CHECK(strstr(out, l), "want \"%s\"", want_net[i]);
  }
  if (fails != bad)
    printf("net said:\n%s", out);
  int pr = peer_test("changeprofile aanetl", SOCK_STREAM, "aanet-l");
  CHECK(pr == EACCES, "connect to a listener whose profile can not accept: %d", pr);
  pr = peer_test("changeprofile aanet", SOCK_DGRAM, "aanet-d1");
  CHECK(pr == 0, "send to a dgram socket whose profile can receive: %d", pr);
  pr = peer_test("changeprofile aanetd", SOCK_DGRAM, "aanet-d2");
  CHECK(pr == EACCES, "send to a dgram socket whose profile can not receive: %d", pr);

  // mount (m2 は決まりのないこちらで tmpfs を)
  CHECK(mount("none", D "/m2", "tmpfs", 0, NULL) == 0, "mount m2: %d", errno);
  collect(self, "mnt", out, sizeof out);
  bad = fails;
  for (size_t i = 0; i < sizeof want_mnt / sizeof *want_mnt; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want_mnt[i]);
    CHECK(strstr(out, l), "want \"%s\"", want_mnt[i]);
  }
  if (fails != bad)
    printf("mnt said:\n%s", out);

  // AF_UNIX の細かい決まり (/tmp/aatest/sock と sock2 は決まりのないこちらで listen)
  {
    struct sockaddr_un p = {.sun_family = AF_UNIX};
    strcpy(p.sun_path, D "/sock");
    int ps = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(ps, (struct sockaddr *)&p, sizeof p) == 0 && listen(ps, 4) == 0, "sock listen: %d", errno);
    strcpy(p.sun_path, D "/sock2");
    int ps2 = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(bind(ps2, (struct sockaddr *)&p, sizeof p) == 0 && listen(ps2, 4) == 0, "sock2 listen: %d", errno);
    struct sockaddr_un ya, za;
    socklen_t yl = abstract(&ya, "aaunix-y"), zl = abstract(&za, "aaunix-z");
    int ys = socket(AF_UNIX, SOCK_STREAM, 0), zs = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(bind(ys, (struct sockaddr *)&ya, yl) == 0 && listen(ys, 4) == 0 && bind(zs, (struct sockaddr *)&za, zl) == 0, "y z: %d", errno);
    collect(self, "unix", out, sizeof out);
    close(ps);
    close(ps2);
    close(ys);
    close(zs);
  }
  bad = fails;
  for (size_t i = 0; i < sizeof want_unix / sizeof *want_unix; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want_unix[i]);
    CHECK(strstr(out, l), "want \"%s\"", want_unix[i]);
  }
  if (fails != bad)
    printf("unix said:\n%s", out);

  // シグナルと ptrace
  sigptrace(out, sizeof out);
  bad = fails;
  for (size_t i = 0; i < sizeof want_sig / sizeof *want_sig; i++) {
    char l[128];
    snprintf(l, sizeof l, "%s\n", want_sig[i]);
    CHECK(strstr(out, l), "want \"%s\"", want_sig[i]);
  }
  if (fails != bad)
    printf("sig said:\n%s", out);

  // 決まりのないプロセスには hat がない
  CHECK(HAT("changehat 1^hat1") == EPERM, "unconfined changehat is EPERM");

  // 動いている LSM: capability がはじめ、AppArmor もある。決まりのないものは unconfined
  unsigned long long ids[16] = {0};
  unsigned isz = sizeof ids;
  long nl = syscall(NR_LSM_LIST, ids, &isz, 0);
  int has_aa = 0;
  for (long i = 0; i < nl; i++)
    has_aa |= ids[i] == 104;
  CHECK(nl >= 2 && isz == nl * 8 && ids[0] == 100 && has_aa, "lsm_list_modules %ld %u %llu", nl, isz, ids[0]);
  isz = 8;
  CHECK(syscall(NR_LSM_LIST, ids, &isz, 0) == -1 && errno == E2BIG && isz == nl * 8, "lsm_list_modules small");
  struct lctx uc = {0};
  unsigned usz = sizeof uc;
  long ur = syscall(NR_LSM_GET, 100, &uc, &usz, 0);
  CHECK(ur == 1 && uc.id == 104 && uc.ctx_len == 10 && !memcmp(uc.ctx, "unconfined", 10) && usz == 48, "lsm_get_self_attr unconfined %ld %llu %u", ur, uc.ctx_len, usz);

  // complain: 決まりの外も読める
  c = fork();
  if (c == 0)
    _exit(puts_("/proc/self/attr/current", "changeprofile aacomplain") ? 2 : op_open(D "/w", O_RDONLY) ? 3 : 0);
  waitpid(c, &st, 0);
  CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "complain allows (%d)", WEXITSTATUS(st));

  // 動いているプロセスのプロファイルを外すと unconfined に
  int go[2], ready[2];
  pipe(go);
  pipe(ready);
  c = fork();
  if (c == 0) {
    char x;
    if (puts_("/proc/self/attr/current", "changeprofile aachild"))
      _exit(2);
    int before = op_open(D "/w", O_RDONLY);
    write(ready[1], "r", 1);
    read(go[0], &x, 1);
    line("/proc/self/attr/current", b, sizeof b);
    _exit(before != EACCES ? 3 : strcmp(b, "unconfined") ? 4 : op_open(D "/w", O_RDONLY) ? 5 : 0);
  }
  char x;
  read(ready[0], &x, 1);
  CHECK(puts_(AA ".remove", "aachild") == 0, ".remove aachild");
  CHECK(puts_(AA ".remove", "aachild") == ENOENT, ".remove again is ENOENT");
  write(go[1], "g", 1);
  waitpid(c, &st, 0);
  CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "removed profile is unconfined (%d)", WEXITSTATUS(st));

  cleanup();
  printf("apparmor: %d failed\n", fails);
  return fails != 0;
}
