// cgroup v2 の pids controller (手本の test/linux.config の CGROUP_PIDS) が Linux と同じか:
//   root      /proc/cgroups の pids の行、根の cgroup.controllers と cgroup.subtree_control、+pids / -pids の書きかた
//             (名前のまちがいは EINVAL、ないところで +pids は ENOENT)、根に pids.* はない
//   files     子の pids.max / current / peak / events / events.local (許可と中身)、cgroup.stat の nr_subsys_pids、
//             pids.max に書く (max、0x、8 進、空白、負は EINVAL、PIDS_MAX から上は EINVAL、あふれは ERANGE)
//   fork      pids.max をこえる fork と pthread_create は EAGAIN、上の cgroup の上限でも EAGAIN、events は上限に
//             当たったところとその上 (events.local はそこだけ)、peak (しくじった fork でも下の peak は上がる)、
//             ゾンビは片づけるまで数える、スレッドも 1 つ、clone3 の CLONE_INTO_CGROUP、kmsg の "fork rejected"
//             (cgroup ごとにはじめの 1 回)
//   migrate   移すときは上限を見ない、共通の祖先の peak が 1 上がる、子に中のものがあれば親へは EBUSY
//   control   中にプロセスがあっても +pids できる (子に中のものがなければ)、子に中のものと自分のプロセスがあれば
//             EBUSY、子が +pids していれば -pids は EBUSY、-pids で pids.* が消える (開いた fd は ENODEV)、+pids
//             しなおすと数えなおし
//   local     pids_localevents でかぶせると、events と events.local は fork にしくじった cgroup の数 (mountinfo にも)
// root で動かす (test/ldiff.sh --root pids)。pid や ino はじかに出さない。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CLONE_INTO_CGROUP 0x200000000ULL

struct clone_args {
  uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup;
};

static const char *M = "/tmp/cgp";
static const char *M2 = "/tmp/cgp2";
static int bad;

static const char *en(int e) {
  switch (e) {
  case 0: return "ok";
  case EPERM: return "EPERM";
  case ENOENT: return "ENOENT";
  case ESRCH: return "ESRCH";
  case EBADF: return "EBADF";
  case EAGAIN: return "EAGAIN";
  case EACCES: return "EACCES";
  case EBUSY: return "EBUSY";
  case EEXIST: return "EEXIST";
  case EINVAL: return "EINVAL";
  case ENODEV: return "ENODEV";
  case EOPNOTSUPP: return "EOPNOTSUPP";
  case ERANGE: return "ERANGE";
  default: {
    static char b[32];
    snprintf(b, sizeof b, "errno %d", e);
    return b;
  }
  }
}

static const char *rc(int r) { return en(r < 0 ? errno : 0); }

static char *path(const char *rel) {
  static char b[4][256];
  static int k;
  k = (k + 1) % 4;
  snprintf(b[k], sizeof b[k], "%s%s%s", M, *rel ? "/" : "", rel);
  return b[k];
}

static const char *rd(const char *p) {
  static char b[4][4096];
  static int k;
  k = (k + 1) % 4;
  int fd = open(p, O_RDONLY);
  if (fd < 0) {
    snprintf(b[k], sizeof b[k], "<%s>", en(errno));
    return b[k];
  }
  ssize_t n = read(fd, b[k], sizeof b[k] - 1);
  int e = errno;
  close(fd);
  if (n < 0) {
    snprintf(b[k], sizeof b[k], "<read %s>", en(e));
    return b[k];
  }
  b[k][n] = 0;
  return b[k];
}

static void show(const char *what, const char *s) {
  printf("%-40s ", what);
  for (; *s; s++)
    putchar(*s == '\n' ? '|' : *s);
  putchar('\n');
}

static void cat(const char *rel) {
  char w[128];
  snprintf(w, sizeof w, "cat %s", *rel ? rel : ".");
  show(w, rd(path(rel)));
}

// 中身のうち、行のはじめの言葉 (key) だけ
static void keys(const char *rel) {
  const char *s = rd(path(rel));
  char b[1024] = "";
  while (*s) {
    const char *sp = strchr(s, ' ');
    const char *nl = strchr(s, '\n');
    if (!nl)
      nl = s + strlen(s);
    strncat(b, s, (sp && sp < nl ? sp : nl) - s);
    strcat(b, " ");
    s = *nl ? nl + 1 : nl;
  }
  char w[128];
  snprintf(w, sizeof w, "keys %s", rel);
  show(w, b);
}

static const char *wr_at(const char *p, const char *s) {
  int fd = open(p, O_WRONLY);
  if (fd < 0)
    return en(errno);
  ssize_t n = write(fd, s, strlen(s));
  int e = errno;
  close(fd);
  if (n < 0)
    return en(e);
  return (size_t)n == strlen(s) ? "ok" : "short";
}

static void wr(const char *rel, const char *s) {
  char w[160], v[64];
  int j = 0;
  for (const char *c = s; *c && j < 60; c++)
    v[j++] = *c == '\n' ? '|' : *c;
  v[j] = 0;
  snprintf(w, sizeof w, "write %s '%s'", rel, v);
  printf("%-40s %s\n", w, wr_at(path(rel), s));
}

// 自分 (と、そのスレッド) を rel へ
static void go(const char *rel) {
  char w[160];
  snprintf(w, sizeof w, "move self to %s", *rel ? rel : ".");
  char p[300];
  snprintf(p, sizeof p, "%s/cgroup.procs", path(rel));
  printf("%-40s %s\n", w, wr_at(p, "0"));
}

static void mv(const char *rel, pid_t pid, const char *who) {
  char b[32], w[160], p[300];
  snprintf(b, sizeof b, "%d", pid);
  snprintf(w, sizeof w, "move %s to %s", who, *rel ? rel : ".");
  snprintf(p, sizeof p, "%s/cgroup.procs", path(rel));
  printf("%-40s %s\n", w, wr_at(p, b));
}

static void mk(const char *rel) {
  char w[160];
  snprintf(w, sizeof w, "mkdir %s", rel);
  printf("%-40s %s\n", w, rc(mkdir(path(rel), 0755)));
}

static void rm(const char *rel) {
  char w[160];
  snprintf(w, sizeof w, "rmdir %s", rel);
  printf("%-40s %s\n", w, rc(rmdir(path(rel))));
}

static void exists(const char *rel) {
  char w[160];
  snprintf(w, sizeof w, "access %s", rel);
  printf("%-40s %s\n", w, rc(access(path(rel), F_OK)));
}

static int cmpstr(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

// ディレクトリの中の名前と許可 (名前の順)
static void ls(const char *rel) {
  DIR *d = opendir(path(rel));
  if (!d) {
    printf("ls %-37s <%s>\n", rel, en(errno));
    return;
  }
  char *names[128];
  int n = 0;
  struct dirent *e;
  while ((e = readdir(d)) && n < 128)
    if (strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
      names[n++] = strdup(e->d_name);
  closedir(d);
  qsort(names, n, sizeof *names, cmpstr);
  printf("ls %s (%d)\n", rel, n);
  for (int i = 0; i < n; i++) {
    char p[300];
    snprintf(p, sizeof p, "%s/%s", path(rel), names[i]);
    struct stat st;
    if (lstat(p, &st) < 0)
      printf("  %-36s <%s>\n", names[i], en(errno));
    else
      printf("  %-36s %c %04o uid=%u\n", names[i], S_ISDIR(st.st_mode) ? 'd' : 'f', st.st_mode & 07777, st.st_uid);
    free(names[i]);
  }
}

// pids の数をまとめて 1 行に (current peak events events.local)
static void pids(const char *rel) {
  char p[4][300];
  const char *f[4] = {"pids.current", "pids.peak", "pids.events", "pids.events.local"};
  char b[512] = "";
  for (int i = 0; i < 4; i++) {
    snprintf(p[i], sizeof p[i], "%s/%s", rel, f[i]);
    const char *s = rd(path(p[i]));
    char t[64];
    snprintf(t, sizeof t, "%s", s);
    t[strcspn(t, "\n")] = 0;
    strcat(b, t);
    if (i < 3)
      strcat(b, " / ");
  }
  char w[128];
  snprintf(w, sizeof w, "pids %s", rel);
  show(w, b);
}

// cgroup.stat を、消えかけ (dying) の css がいなくなってから (Linux は RCU のあとに片づける)
static void stat_settled(const char *rel) {
  char p[300];
  snprintf(p, sizeof p, "%s%scgroup.stat", rel, *rel ? "/" : "");
  usleep(100000);
  for (int i = 0; i < 100; i++) {
    const char *s = rd(path(p));
    if (strstr(s, "nr_dying_descendants 0") && (!strstr(s, "nr_dying_subsys_pids") || strstr(s, "nr_dying_subsys_pids 0")))
      break;
    usleep(20000);
  }
  cat(p);
}

// 止めておく子 (SIGKILL で終わる)
static pid_t sleeper(void) {
  fflush(stdout);
  pid_t p = fork();
  if (p == 0) {
    for (;;)
      pause();
  }
  return p;
}

static void reap(pid_t p) {
  int st;
  kill(p, SIGKILL);
  waitpid(p, &st, 0);
}

// fork してすぐ終わる子 (できれば片づける)
static void try_fork(const char *what) {
  fflush(stdout);
  pid_t p = fork();
  if (p == 0)
    _exit(0);
  int e = errno;
  if (p > 0)
    waitpid(p, 0, 0);
  printf("%-40s %s\n", what, p < 0 ? en(e) : "ok");
}

static volatile int thr_done;
static void *thr(void *a) {
  (void)a;
  while (!__atomic_load_n(&thr_done, __ATOMIC_SEQ_CST))
    usleep(1000);
  return 0;
}

// pids.current が want になるまで待つ (スレッドや子が片づくのは、join や wait から戻ったあとのこともある)
static void wait_current(const char *rel, const char *want) {
  char p[300];
  snprintf(p, sizeof p, "%s/pids.current", rel);
  for (int i = 0; i < 200 && strcmp(rd(path(p)), want); i++)
    usleep(10000);
}

// kmsg の "fork rejected by pids controller in PATH" (前に数えた数から増えたものだけ)
static int kmsg_count(const char *cg) {
  static char buf[1 << 20];
  int n = klogctl(3, buf, sizeof buf - 1);
  if (n < 0)
    return -1;
  buf[n] = 0;
  char want[160];
  snprintf(want, sizeof want, "cgroup: fork rejected by pids controller in %s\n", cg);
  int c = 0;
  for (char *s = buf; (s = strstr(s, want)); s += strlen(want))
    c++;
  return c;
}

static void kmsg_since(const char *cg, int before) {
  int now = kmsg_count(cg);
  char w[160];
  snprintf(w, sizeof w, "kmsg fork rejected in %s", cg);
  if (now < 0 || before < 0)
    printf("%-40s <%s>\n", w, en(errno));
  else
    printf("%-40s +%d\n", w, now - before);
}

static void sec_root(void) {
  puts("== root");
  // /proc/cgroups の pids の行 (num_cgroups は VM でちがうので出さない)
  {
    const char *s = rd("/proc/cgroups");
    char b[512] = "";
    for (const char *l = s; *l;) {
      const char *nl = strchr(l, '\n');
      if (!nl)
        nl = l + strlen(l);
      char line[128];
      snprintf(line, sizeof line, "%.*s", (int)(nl - l), l);
      if (!strncmp(line, "pids\t", 5)) {
        char name[32];
        int hier, num, en_;
        if (sscanf(line, "%31s %d %d %d", name, &hier, &num, &en_) == 4)
          snprintf(line, sizeof line, "%s hierarchy=%d enabled=%d", name, hier, en_);
        strcat(b, line);
        strcat(b, ";");
      } else if (line[0] == '#') {
        strcat(b, line);
        strcat(b, ";");
      }
      l = *nl ? nl + 1 : nl;
    }
    show("/proc/cgroups", b);
  }
  mkdir(M, 0755);
  printf("%-40s %s\n", "mount cgroup2", rc(mount("none", M, "cgroup2", 0, 0)));
  cat("cgroup.controllers");
  cat("cgroup.subtree_control");
  keys("cgroup.stat");
  exists("pids.max");
  exists("pids.current");
  wr("cgroup.subtree_control", "pids");
  wr("cgroup.subtree_control", "+nope");
  wr("cgroup.subtree_control", "*pids");
  wr("cgroup.subtree_control", "+pids -pids");
  cat("cgroup.subtree_control");
  wr("cgroup.subtree_control", "  +pids  \n");
  cat("cgroup.subtree_control");
  wr("cgroup.subtree_control", "+pids");
  wr("cgroup.subtree_control", "+memory");
  exists("pids.max");
}

static void sec_files(void) {
  puts("== files");
  mk("pt");
  ls("pt");
  cat("pt/cgroup.controllers");
  cat("pt/cgroup.subtree_control");
  cat("pt/pids.max");
  pids("pt");
  cat("pt/cgroup.stat");
  wr("pt/pids.max", "10");
  cat("pt/pids.max");
  wr("pt/pids.max", "max");
  cat("pt/pids.max");
  wr("pt/pids.max", "0x10");
  cat("pt/pids.max");
  wr("pt/pids.max", "010");
  cat("pt/pids.max");
  wr("pt/pids.max", " 7 \n");
  cat("pt/pids.max");
  wr("pt/pids.max", "0");
  cat("pt/pids.max");
  wr("pt/pids.max", "4194304");
  cat("pt/pids.max");
  wr("pt/pids.max", "4194305");
  wr("pt/pids.max", "-1");
  wr("pt/pids.max", "abc");
  wr("pt/pids.max", "12abc");
  wr("pt/pids.max", "");
  wr("pt/pids.max", "99999999999999999999");
  wr("pt/pids.max", "max\n");
  cat("pt/pids.max");
  wr("pt/pids.current", "1");
  wr("pt/pids.peak", "1");
  wr("pt/pids.events", "max 0");
  // 子は、pt が +pids するまで pids を持たない (ないものを +pids は ENOENT)
  mk("pt/a");
  cat("pt/a/cgroup.controllers");
  exists("pt/a/pids.max");
  mk("pt/a/x");
  wr("pt/a/x/cgroup.subtree_control", "+pids");
  rm("pt/a/x");
  wr("pt/a/cgroup.subtree_control", "+pids");
  wr("pt/cgroup.subtree_control", "+pids");
  cat("pt/cgroup.subtree_control");
  cat("pt/a/cgroup.controllers");
  exists("pt/a/pids.max");
  mk("pt/b");
  exists("pt/b/pids.max");
  pids("pt/a");
  pids("pt/b");
  stat_settled("pt");
  keys("cgroup.stat");
}

static void sec_fork(void) {
  puts("== fork");
  int k_a = kmsg_count("/pt/a"), k_b = kmsg_count("/pt/b");
  go("pt/a");
  pids("pt");
  pids("pt/a");
  wr("pt/a/pids.max", "3");
  pid_t s1 = sleeper(), s2 = sleeper();
  pids("pt/a");
  try_fork("fork over pt/a pids.max");
  pids("pt/a");
  pids("pt");
  {
    pthread_t t;
    int r = pthread_create(&t, 0, thr, 0);
    printf("%-40s %s\n", "pthread_create over pids.max", en(r));
    if (r == 0) {
      thr_done = 1;
      pthread_join(t, 0);
    }
  }
  pids("pt/a");
  try_fork("fork over pt/a pids.max again");
  kmsg_since("/pt/a", k_a);
  // 上の cgroup (pt) の上限: pt/a の peak は上がる (Linux の pids_try_charge は下から数え、上でしくじっても戻さない)
  wr("pt/a/pids.max", "max");
  wr("pt/pids.max", "3");
  try_fork("fork over pt pids.max");
  pids("pt/a");
  pids("pt");
  kmsg_since("/pt/a", k_a);
  // 上限を今の数より下に: 移すのは止めないが、fork は止める
  wr("pt/pids.max", "max");
  wr("pt/a/pids.max", "1");
  try_fork("fork with pids.max < current");
  pids("pt/a");
  wr("pt/a/pids.max", "max");
  // ゾンビは片づけるまで数える
  fflush(stdout);
  pid_t z = fork();
  if (z == 0)
    _exit(0);
  for (int i = 0; i < 200; i++) {
    siginfo_t si = {0};
    if (waitid(P_PID, z, &si, WEXITED | WNOHANG | WNOWAIT) == 0 && si.si_pid == z)
      break;
    usleep(10000);
  }
  pids("pt/a");
  waitpid(z, 0, 0);
  wait_current("pt/a", "3\n");
  pids("pt/a");
  // スレッドも 1 つ
  thr_done = 0;
  pthread_t t1, t2;
  int r1 = pthread_create(&t1, 0, thr, 0), r2 = pthread_create(&t2, 0, thr, 0);
  printf("%-40s %s %s\n", "pthread_create x2", en(r1), en(r2));
  pids("pt/a");
  thr_done = 1;
  if (!r1)
    pthread_join(t1, 0);
  if (!r2)
    pthread_join(t2, 0);
  wait_current("pt/a", "3\n");
  pids("pt/a");
  // clone3 の CLONE_INTO_CGROUP: 行き先の上限で EAGAIN (fork にしくじったのは行き先の cgroup)
  wr("pt/b/pids.max", "0");
  int bfd = open(path("pt/b"), O_RDONLY | O_DIRECTORY);
  struct clone_args ca = {.flags = CLONE_INTO_CGROUP, .exit_signal = SIGCHLD, .cgroup = (uint64_t)bfd};
  fflush(stdout);
  long c = syscall(SYS_clone3, &ca, sizeof ca);
  if (c == 0)
    _exit(0);
  int e = errno;
  if (c > 0)
    waitpid(c, 0, 0);
  printf("%-40s %s\n", "clone3 into pt/b (pids.max 0)", c < 0 ? en(e) : "ok");
  pids("pt/b");
  pids("pt/a");
  pids("pt");
  kmsg_since("/pt/b", k_b);
  wr("pt/b/pids.max", "1");
  fflush(stdout);
  c = syscall(SYS_clone3, &ca, sizeof ca);
  if (c == 0)
    _exit(0);
  e = errno;
  if (c > 0)
    waitpid(c, 0, 0);
  printf("%-40s %s\n", "clone3 into pt/b (pids.max 1)", c < 0 ? en(e) : "ok");
  wait_current("pt/b", "0\n");
  pids("pt/b");
  close(bfd);
  wr("pt/b/pids.max", "max");

  puts("== migrate");
  // 移すときは上限を見ない
  wr("pt/b/pids.max", "0");
  mv("pt/b", s1, "sleeper 1");
  pids("pt/b");
  pids("pt/a");
  pids("pt");
  wr("pt/b/pids.max", "max");
  // 共通の祖先 (pm) は、行き先に足してからもとを引くので peak が 1 上がる。pids を分けあうところ (pm/b と pm/b/q) の
  // あいだでは数は動かない
  mk("pm");
  wr("pm/cgroup.subtree_control", "+pids");
  mk("pm/a");
  mk("pm/b");
  mk("pm/b/q");
  exists("pm/b/q/pids.max");
  pid_t s4 = sleeper();
  mv("pm/a", s4, "sleeper 4");
  pids("pm");
  pids("pm/a");
  mv("pm/b", s4, "sleeper 4");
  pids("pm");
  pids("pm/a");
  pids("pm/b");
  mv("pm/b/q", s4, "sleeper 4");
  pids("pm");
  pids("pm/b");
  mv("pm/b", s4, "sleeper 4");
  pids("pm/b");
  stat_settled("pm");
  // pt は +pids していて、子 (pt/a、pt/b) に中のものがある: pt へは EBUSY
  mv("pt", s2, "sleeper 2");
  mk("pt/c");
  mv("pt/c", s2, "sleeper 2");
  pids("pt/c");
  pids("pt/a");
  pids("pt");
  mv("pt/a", s2, "sleeper 2");

  puts("== control");
  // pt/a は自分 (と sleeper 2) がいて、子はない: +pids できる
  wr("pt/a/cgroup.subtree_control", "+pids");
  mk("pt/a/x");
  exists("pt/a/x/pids.max");
  mv("pt/a/x", s2, "sleeper 2");
  pids("pt/a/x");
  pids("pt/a");
  // pt/c は子 (pt/c/y) に中のものがいて、自分にもいる: +pids は EBUSY
  mk("pt/c/y");
  mv("pt/c/y", s1, "sleeper 1");
  pid_t s3 = sleeper();
  mv("pt/c", s3, "sleeper 3");
  wr("pt/c/cgroup.subtree_control", "+pids");
  pids("pt/c");
  // 子が +pids しているので -pids は EBUSY
  wr("pt/cgroup.subtree_control", "-pids");
  // -pids で pids.* が消え、開いていた fd は ENODEV
  int fd = open(path("pt/a/x/pids.max"), O_RDONLY);
  wr("pt/a/cgroup.subtree_control", "-pids");
  exists("pt/a/x/pids.max");
  {
    char b[64];
    ssize_t n = read(fd, b, sizeof b);
    printf("%-40s %s\n", "read pids.max after -pids", n < 0 ? en(errno) : "data");
    close(fd);
  }
  cat("pt/a/x/cgroup.controllers");
  pids("pt/a");
  stat_settled("pt/a");
  stat_settled("pt");
  // -pids と +pids しなおし: 数えなおし (events は 0、peak はいまの数から)
  pids("pt");
  wr("pt/cgroup.subtree_control", "-pids");
  exists("pt/a/pids.max");
  pids("pt");
  stat_settled("pt");
  wr("pt/cgroup.subtree_control", "+pids");
  pids("pt/a");
  pids("pt/c");
  pids("pt");
  cat("pt/a/pids.max");
  stat_settled("pt");

  puts("== local");
  // pids_localevents: events も events.local も、fork にしくじった cgroup (pt/a) の数
  mkdir(M2, 0755);
  printf("%-40s %s\n", "mount cgroup2 pids_localevents", rc(mount("none", M2, "cgroup2", 0, "pids_localevents")));
  {
    FILE *f = fopen("/proc/self/mountinfo", "r");
    char l[512];
    while (f && fgets(l, sizeof l, f)) {
      char *p = strstr(l, " /tmp/cgp2 ");
      char *o = p ? strstr(p, " - ") : 0;
      if (o) {
        l[strcspn(l, "\n")] = 0;
        show("mountinfo /tmp/cgp2", o + 3);
      }
    }
    if (f)
      fclose(f);
  }
  wr("pt/pids.max", "3");
  try_fork("fork over pt pids.max (local)");
  pids("pt/a");
  pids("pt");
  wr("pt/a/pids.max", "2");
  try_fork("fork over pt/a pids.max (local)");
  pids("pt/a");
  pids("pt");
  // pids_localevents なしでかぶせなおすと、もとの見え方
  umount(M2);
  printf("%-40s %s\n", "mount cgroup2 again (no option)", rc(mount("none", M2, "cgroup2", 0, 0)));
  pids("pt/a");
  pids("pt");
  umount(M2);
  rmdir(M2);
  wr("pt/pids.max", "max");
  wr("pt/a/pids.max", "max");

  puts("== cleanup");
  reap(s1);
  reap(s2);
  reap(s3);
  reap(s4);
  go("");
  wait_current("pt", "0\n");
  wait_current("pm", "0\n");
  pids("pt");
  pids("pm");
  rm("pt/a/x");
  rm("pt/a");
  rm("pt/b");
  rm("pt/c/y");
  rm("pt/c");
  rm("pm/b/q");
  rm("pm/b");
  rm("pm/a");
  rm("pm");
  // 子 (pt) が +pids しているので、根の -pids は EBUSY
  wr("cgroup.subtree_control", "-pids");
  rm("pt");
  wr("cgroup.subtree_control", "-pids");
  cat("cgroup.subtree_control");
}

int main(int argc, char **argv) {
  if (argc > 1 && !strcmp(argv[1], "console")) {
    int c = open("/dev/console", O_WRONLY);
    if (c >= 0)
      dup2(c, 1);
  }
  setvbuf(stdout, 0, _IOLBF, 0);
  if (getuid() != 0) {
    puts("run as root (test/ldiff.sh --root pids)");
    return 2;
  }
  sec_root();
  sec_files();
  // fork などは子で (移るのは子だけ: このプロセスの cgroup はそのまま)
  fflush(stdout);
  pid_t k = fork();
  if (k == 0) {
    sec_fork();
    fflush(stdout);
    _exit(bad);
  }
  int st = 0;
  waitpid(k, &st, 0);
  if (!WIFEXITED(st) || WEXITSTATUS(st))
    bad++;
  printf("%-40s %s\n", "umount", rc(umount(M)));
  rmdir(M);
  return bad;
}
