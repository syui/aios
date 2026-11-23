// ユーザーの namespace の中の capability が Linux と同じか (root でない人で。引数は書けるディレクトリ: @home):
//   before   はじめの 5 つの組 (/proc/self/status: root でない人は bounding だけ)
//   unshare  CLONE_NEWUSER のすぐあと: 中ではみな持つ (Linux の set_cred_user_ns: inheritable と ambient は空、permitted と
//            effective と bounding はみな)。uid は 65534 (写していない)。capget も
//   map      自分の uid / gid を中の 0 に
//   unmapped 写す前は、自分のものでも mode 000 のファイルは読めない (持ち主の uid が中に写っていないので CAP_DAC_OVERRIDE
//            は効かない: Linux の privileged_wrt_inode_uidgid)
//   init     はじめの namespace のもの (sethostname、settimeofday、装置の mknod) は、中の capability ではできない (EPERM)
//   own      中のもの: 新しい UTS の namespace の sethostname、自分のもの (中に写した uid) で mode 000 のファイルを読む
//            (CAP_DAC_OVERRIDE)
//   capset   CAP_SYS_ADMIN を permitted と effective から落とす、PR_CAPBSET_DROP で CAP_NET_RAW を bounding から
//   exec     中の root (uid 0) が exec すると permitted は bounding (と inheritable)、effective も
//   noroot   SECBIT_NOROOT をつけて exec すると、中の root でも何も持たない
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static int bad;

static const char *en(int r) {
  static char b[32];
  if (r >= 0)
    return "0";
  switch (errno) {
  case EPERM:
    return "EPERM";
  case EACCES:
    return "EACCES";
  case EINVAL:
    return "EINVAL";
  case ENOENT:
    return "ENOENT";
  }
  snprintf(b, sizeof b, "errno%d", errno);
  return b;
}

// /proc/self/status の Cap 行 (5 つを 1 行に)
static void status(const char *what) {
  FILE *f = fopen("/proc/self/status", "r");
  char l[256];
  printf("%s:", what);
  while (f && fgets(l, sizeof l, f)) {
    if (strncmp(l, "Cap", 3) == 0) {
      char k[16];
      unsigned long long v;
      if (sscanf(l, "%15[^:]:%llx", k, &v) == 2)
        printf(" %s %llx", k + 3, v);
    }
  }
  if (f)
    fclose(f);
  printf("\n");
}

static void capget3(const char *what) {
  struct __user_cap_header_struct h = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct d[2];
  memset(d, 0, sizeof d);
  int r = syscall(SYS_capget, &h, d);
  printf("%s: capget %s eff %llx prm %llx inh %llx\n", what, en(r), (unsigned long long)d[1].effective << 32 | d[0].effective,
         (unsigned long long)d[1].permitted << 32 | d[0].permitted, (unsigned long long)d[1].inheritable << 32 | d[0].inheritable);
}

static int put(const char *file, const char *s) {
  int fd = open(file, O_WRONLY);
  if (fd < 0)
    return -1;
  int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1;
  close(fd);
  return r;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // exec のあと: 組を見せて終わる
  if (argc == 3 && strcmp(argv[1], "--show") == 0) {
    status(argv[2]);
    return 0;
  }
  const char *dir = argc > 1 ? argv[1] : ".";
  uid_t me = geteuid();
  gid_t mygid = getegid();
  status("before");
  char p[512];
  snprintf(p, sizeof p, "%s/capsns-file", dir);
  unlink(p);
  int fd = open(p, O_CREAT | O_WRONLY, 0600);
  if (fd >= 0) {
    write(fd, "x", 1);
    close(fd);
  }
  chmod(p, 0);
  pid_t c = fork();
  if (c == 0) {
    int r = unshare(CLONE_NEWUSER);
    printf("unshare: %s, uid %u\n", en(r), (unsigned)getuid());
    status("unshare");
    capget3("unshare");
    int ur = open(p, O_RDONLY);
    printf("unmapped: mode 000 own file read %s\n", en(ur));
    if (ur >= 0)
      close(ur);
    char m[64];
    snprintf(m, sizeof m, "0 %u 1\n", (unsigned)me);
    int a = put("/proc/self/uid_map", m);
    int b = put("/proc/self/setgroups", "deny");
    snprintf(m, sizeof m, "0 %u 1\n", (unsigned)mygid);
    int g = put("/proc/self/gid_map", m);
    printf("map: uid_map %s setgroups %s gid_map %s, uid %u gid %u\n", en(a), en(b), en(g), (unsigned)getuid(), (unsigned)getgid());
    // はじめの namespace のもの
    int h = sethostname("capsns", 6);
    printf("init: sethostname %s", en(h));
    struct timeval tv;
    gettimeofday(&tv, NULL);
    printf(", settimeofday %s", en(settimeofday(&tv, NULL)));
    char n[512];
    snprintf(n, sizeof n, "%s/capsns-null", dir);
    unlink(n);
    printf(", mknod chr %s\n", en(mknod(n, S_IFCHR | 0600, makedev(1, 3))));
    unlink(n);
    // 中のもの
    int rd = open(p, O_RDONLY);
    printf("own: mode 000 own file read %s", en(rd));
    if (rd >= 0)
      close(rd);
    pid_t u = fork();
    if (u == 0) {
      int x = unshare(CLONE_NEWUTS);
      printf(", unshare uts %s, sethostname %s\n", en(x), en(sethostname("capsns", 6)));
      _exit(0);
    }
    waitpid(u, NULL, 0);
    // capset と bounding
    struct __user_cap_header_struct hd = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct d[2];
    syscall(SYS_capget, &hd, d);
    d[CAP_SYS_ADMIN / 32].effective &= ~(1u << (CAP_SYS_ADMIN % 32));
    d[CAP_SYS_ADMIN / 32].permitted &= ~(1u << (CAP_SYS_ADMIN % 32));
    int s = syscall(SYS_capset, &hd, d);
    int bd = prctl(PR_CAPBSET_DROP, CAP_NET_RAW, 0, 0, 0);
    printf("capset: drop sys_admin %s, bset drop net_raw %s, bset read net_raw %d sys_admin %d\n", en(s), en(bd), prctl(PR_CAPBSET_READ, CAP_NET_RAW, 0, 0, 0),
           prctl(PR_CAPBSET_READ, CAP_SYS_ADMIN, 0, 0, 0));
    status("capset");
    // 中の root の exec
    pid_t e = fork();
    if (e == 0) {
      execl("/proc/self/exe", "capsns", "--show", "exec", (char *)0);
      _exit(9);
    }
    waitpid(e, NULL, 0);
    // SECBIT_NOROOT (CAP_SETPCAP がいる)
    int sb = prctl(PR_SET_SECUREBITS, 1 << 0, 0, 0, 0);
    printf("noroot: set securebits %s\n", en(sb));
    e = fork();
    if (e == 0) {
      execl("/proc/self/exe", "capsns", "--show", "noroot", (char *)0);
      _exit(9);
    }
    waitpid(e, NULL, 0);
    _exit(0);
  }
  int st;
  waitpid(c, &st, 0);
  unlink(p);
  status("after");
  printf("capsns: %d bad\n", bad);
  return bad;
}
