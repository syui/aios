// cgroup v2 の核 (controller なし: 手本の test/linux.config の CGROUPS だけ) が Linux と同じか:
//   proc      /proc/self/cgroup と task/TID/cgroup、/proc/cgroups、/proc/filesystems の cgroup と cgroup2、
//             /proc/self/ns/cgroup、/sys/fs/cgroup、/sys/kernel/cgroup の delegate と features
//   mount     cgroup2 をかぶせる: statfs の型、mountinfo、根のファイルの名前と許可と中身
//   mkdir     子の cgroup: ファイル、cgroup.type、events、stat、max.depth と max.descendants の上限、rmdir の EBUSY
//   procs     cgroup.procs に書いて移す (0 は自分、スレッドの番号はそのプロセスごと、ないものは ESRCH)、fork の子と
//             スレッドは同じ cgroup、cgroup.threads (domain では EOPNOTSUPP)、populated
//   kill      cgroup.kill で中のものに SIGKILL
//   freeze    cgroup.freeze: 中と子孫が凍る (events の frozen、State は S、ふつうのシグナルはとけるまで待つ、SIGKILL は効く)、
//             凍った cgroup へ移ると凍り、出るととける、空の cgroup も凍る
//   clone3    CLONE_INTO_CGROUP (cgroup の fd、ちがう fd は EBADF)
//   ns        unshare(CLONE_NEWCGROUP) のあとの /proc/self/cgroup と、外のものの見えかた、中でかぶせた cgroup2 の根、
//             setns でもどる、NS_GET_NSTYPE
//   pidfd     PIDFD_GET_INFO の cgroupid が cgroup のディレクトリの ino と同じ
//   perm      ふつうの人 (nobody) は書けない・作れない。chown して任せた (delegate) ところでは、共通の祖先の cgroup.procs に
//             書けなければ移れない
// root で動かす (test/ldiff.sh --root cgroup)。番号 (pid、ino、ns の番号) はじかに出さず、あてはまるかを出す。
// 終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef CLONE_NEWCGROUP
#define CLONE_NEWCGROUP 0x02000000
#endif
#define CLONE_INTO_CGROUP 0x200000000ULL
#define NS_GET_NSTYPE 0xb703
#define CGROUP2_SUPER_MAGIC 0x63677270

struct clone_args {
  uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup;
};

struct pidfd_info {
  uint64_t mask, cgroupid;
  uint32_t pid, tgid, ppid, ruid, rgid, euid, egid, suid, sgid, fsuid, fsgid;
  int32_t exit_code;
};
#define PIDFD_INFO_CGROUPID (1UL << 2)
#define PIDFD_GET_INFO _IOWR(0xFF, 11, struct pidfd_info)

static const char *M = "/tmp/cgt";
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
  case ENOTDIR: return "ENOTDIR";
  case EISDIR: return "EISDIR";
  case EINVAL: return "EINVAL";
  case ENODEV: return "ENODEV";
  case ENOSPC: return "ENOSPC";
  case EROFS: return "EROFS";
  case ENOTEMPTY: return "ENOTEMPTY";
  case EOPNOTSUPP: return "EOPNOTSUPP";
  case ENOSYS: return "ENOSYS";
  case E2BIG: return "E2BIG";
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

// ファイルを読む (改行は \n のまま)。だめなら errno の名前
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

// 1 行にして出す (改行は | に)
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

// 中身のうち、行のはじめの言葉 (key) だけ (値はタイミングでちがうもの: cpu.stat)
static void keys(const char *rel) {
  const char *s = rd(path(rel));
  char b[1024] = "";
  while (*s) {
    const char *sp = strchr(s, ' ');
    const char *nl = strchr(s, '\n');
    if (!nl)
      nl = s + strlen(s);
    if (sp && sp < nl)
      strncat(b, s, sp - s);
    else
      strncat(b, s, nl - s);
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
  if ((size_t)n != strlen(s))
    return "short";
  return "ok";
}

static void wr(const char *rel, const char *s) {
  char w[160];
  snprintf(w, sizeof w, "write %s %s", rel, s);
  const char *r = wr_at(path(rel), s);
  printf("%-40s %s\n", w, r);
}

static void wrn(const char *rel, long n) {
  char b[32];
  snprintf(b, sizeof b, "%ld", n);
  char w[160];
  snprintf(w, sizeof w, "write %s <pid>", rel);
  printf("%-40s %s\n", w, wr_at(path(rel), b));
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

static int cmpstr(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

// ディレクトリの中の名前と許可と型を名前の順に
static void ls(const char *rel) {
  DIR *d = opendir(path(rel));
  if (!d) {
    printf("ls %-37s <%s>\n", *rel ? rel : ".", en(errno));
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
  printf("ls %s (%d)\n", *rel ? rel : ".", n);
  for (int i = 0; i < n; i++) {
    char p[300];
    snprintf(p, sizeof p, "%s/%s", path(rel), names[i]);
    struct stat st;
    if (lstat(p, &st) < 0)
      printf("  %-36s <%s>\n", names[i], en(errno));
    else
      printf("  %-36s %c %04o nlink=%lu size=%ld uid=%u\n", names[i], S_ISDIR(st.st_mode) ? 'd' : S_ISREG(st.st_mode) ? 'f' : '?',
             st.st_mode & 07777, S_ISDIR(st.st_mode) ? (unsigned long)st.st_nlink : 1UL, (long)st.st_size, st.st_uid);
    free(names[i]);
  }
}

// cgroup.procs に pid があるか、いくつか
static void has(const char *rel, pid_t pid) {
  const char *s = rd(path(rel));
  int n = 0, found = 0;
  for (const char *l = s; *l;) {
    if (*l == '<') {
      printf("procs %-34s %s\n", rel, s);
      return;
    }
    n++;
    if (atol(l) == pid)
      found = 1;
    l = strchr(l, '\n');
    if (!l)
      break;
    l++;
  }
  printf("procs %-34s %s%s\n", rel, found ? "has" : "lacks", n == 0 ? " (empty)" : "");
}

static void self_cg(const char *what) { show(what, rd("/proc/self/cgroup")); }

static pid_t tid(void) { return syscall(SYS_gettid); }

// ---- スレッド ----
static volatile int thr_go, thr_done;
static pid_t thr_tid;
static void *thr(void *a) {
  (void)a;
  thr_tid = tid();
  __atomic_store_n(&thr_go, 1, __ATOMIC_SEQ_CST);
  while (!__atomic_load_n(&thr_done, __ATOMIC_SEQ_CST))
    usleep(1000);
  return 0;
}

static void task_cg(const char *what, pid_t t) {
  char p[64];
  snprintf(p, sizeof p, "/proc/self/task/%d/cgroup", t);
  show(what, rd(p));
}

// 子を作り、止めておく (k を書けば終わる)
static pid_t sleeper(void) {
  pid_t p = fork();
  if (p == 0) {
    for (;;)
      pause();
  }
  return p;
}

static void reap(const char *what, pid_t p) {
  int st, r = 0;
  for (int i = 0; i < 200 && (r = waitpid(p, &st, WNOHANG)) == 0; i++)
    usleep(10000);
  if (r == 0) {
    kill(p, SIGKILL);
    waitpid(p, &st, 0);
    bad++;
    printf("%-40s timeout\n", what);
  } else if (r < 0)
    printf("%-40s <%s>\n", what, en(errno));
  else if (WIFSIGNALED(st))
    printf("%-40s signal %d\n", what, WTERMSIG(st));
  else
    printf("%-40s exit %d\n", what, WEXITSTATUS(st));
}

static void sec_proc(void) {
  puts("== proc");
  // cgroup2 をはじめてかぶせるまで、ない (Linux の cgrp_dfl_visible)。aios の VM は同じものでテストを続けるので、前に
  // かぶせていれば 0::/ (どちらでもよい)
  const char *s = rd("/proc/self/cgroup");
  show("/proc/self/cgroup", !*s || !strcmp(s, "0::/\n") ? "empty or 0::/" : s);
  char tp[64];
  snprintf(tp, sizeof tp, "/proc/self/task/%d/cgroup", tid());
  s = rd(tp);
  show("task/TID/cgroup", !*s || !strcmp(s, "0::/\n") ? "empty or 0::/" : s);
  show("/proc/cgroups", rd("/proc/cgroups"));
  {
    FILE *f = fopen("/proc/filesystems", "r");
    char l[128], b[256] = "";
    while (f && fgets(l, sizeof l, f))
      if (strstr(l, "cgroup")) {
        l[strcspn(l, "\n")] = 0;
        strcat(b, l);
        strcat(b, ";");
      }
    if (f)
      fclose(f);
    show("/proc/filesystems cgroup", b);
  }
  char l[128] = "";
  ssize_t n = readlink("/proc/self/ns/cgroup", l, sizeof l - 1);
  if (n >= 0) {
    l[n] = 0;
    *strchrnul(l, '[') = 0;
    show("readlink ns/cgroup", l);
  } else
    show("readlink ns/cgroup", en(errno));
  struct stat st;
  printf("%-40s %s\n", "stat /sys/fs/cgroup",
         stat("/sys/fs/cgroup", &st) < 0 ? en(errno) : S_ISDIR(st.st_mode) ? "dir" : "not dir");
  show("/sys/kernel/cgroup/delegate", rd("/sys/kernel/cgroup/delegate"));
  show("/sys/kernel/cgroup/features", rd("/sys/kernel/cgroup/features"));
  // /proc/self に cgroup がある (readdir で)
  DIR *d = opendir("/proc/self");
  int found = 0;
  struct dirent *e;
  while (d && (e = readdir(d)))
    if (!strcmp(e->d_name, "cgroup"))
      found = 1;
  if (d)
    closedir(d);
  printf("%-40s %s\n", "readdir /proc/self cgroup", found ? "yes" : "no");
  struct stat cs;
  printf("%-40s %s %04o\n", "stat /proc/self/cgroup", rc(stat("/proc/self/cgroup", &cs)), cs.st_mode & 07777);
}

static void sec_mount(void) {
  puts("== mount");
  mkdir(M, 0755);
  printf("%-40s %s\n", "mount cgroup2", rc(mount("none", M, "cgroup2", 0, 0)));
  // 同じところにもう一度は EBUSY (かぶせたら外す)
  int again = mount("none", M, "cgroup2", 0, 0);
  printf("%-40s %s\n", "mount cgroup2 again", rc(again));
  if (again == 0)
    umount(M);
  self_cg("/proc/self/cgroup after mount");
  int bo = mount("none", M, "cgroup2", 0, "nosuchopt");
  printf("%-40s %s\n", "mount cgroup2 bad option", rc(bo));
  if (bo == 0)
    umount(M);
  struct statfs sf;
  if (statfs(M, &sf) == 0)
    printf("%-40s %lx bsize=%ld namelen=%ld\n", "statfs", (long)sf.f_type, (long)sf.f_bsize, (long)sf.f_namelen);
  else
    printf("%-40s %s\n", "statfs", en(errno));
  FILE *f = fopen("/proc/self/mountinfo", "r");
  char l[512];
  while (f && fgets(l, sizeof l, f)) {
    char *p = strstr(l, " /tmp/cgt ");
    if (!p)
      continue;
    // マウント先から先 (番号は外す)
    l[strcspn(l, "\n")] = 0;
    show("mountinfo", p + 1);
  }
  if (f)
    fclose(f);
  struct stat st;
  stat(M, &st);
  printf("%-40s %s %04o uid=%u nlink=%lu\n", "stat root", st.st_ino == 1 ? "ino=1" : "ino!=1", st.st_mode & 07777, st.st_uid,
         (unsigned long)st.st_nlink);
  ls("");
  cat("cgroup.controllers");
  cat("cgroup.subtree_control");
  cat("cgroup.max.depth");
  cat("cgroup.max.descendants");
  cat("cgroup.stat");
  cat("cgroup.type");
  cat("cgroup.events");
  cat("cgroup.freeze");
  keys("cpu.stat");
  has("cgroup.procs", getpid());
  has("cgroup.threads", tid());
  wr("cgroup.subtree_control", "+memory");
  wr("cgroup.subtree_control", "");
  wr("cgroup.max.depth", "5");
  wr("cgroup.controllers", "x");
}

static void sec_mkdir(void) {
  puts("== mkdir");
  mk("a");
  mk("a");
  ls("a");
  struct stat st;
  stat(path("a"), &st);
  printf("%-40s %04o nlink=%lu\n", "stat a", st.st_mode & 07777, (unsigned long)st.st_nlink);
  stat(M, &st);
  printf("%-40s nlink=%lu\n", "stat root after mkdir", (unsigned long)st.st_nlink);
  cat("a/cgroup.type");
  cat("a/cgroup.events");
  cat("a/cgroup.freeze");
  cat("a/cgroup.max.depth");
  cat("a/cgroup.max.descendants");
  cat("a/cgroup.stat");
  cat("a/cgroup.stat.local");
  cat("a/cgroup.controllers");
  cat("a/cgroup.subtree_control");
  keys("a/cpu.stat");
  keys("a/cpu.stat.local");
  cat("a/cgroup.procs");
  cat("cgroup.stat");
  // ファイルは作れない、名前は変えられない
  printf("%-40s %s\n", "creat a/x", rc(open(path("a/x"), O_CREAT | O_WRONLY, 0644)));
  printf("%-40s %s\n", "rename a z", rc(rename(path("a"), path("z"))));
  printf("%-40s %s\n", "unlink a/cgroup.procs", rc(unlink(path("a/cgroup.procs"))));
  printf("%-40s %s\n", "chmod a/cgroup.procs 0600", rc(chmod(path("a/cgroup.procs"), 0600)));
  stat(path("a/cgroup.procs"), &st);
  printf("%-40s %04o\n", "stat a/cgroup.procs", st.st_mode & 07777);
  chmod(path("a/cgroup.procs"), 0644);
  mk("a/b");
  mk("a/b/c");
  rm("a");
  rm("a/b");
  cat("a/cgroup.stat");
  wr("a/cgroup.max.depth", "1");
  cat("a/cgroup.max.depth");
  mk("a/b/d");
  mk("a/e");
  wr("a/cgroup.max.descendants", "2");
  cat("a/cgroup.max.descendants");
  mk("a/f");
  wr("a/cgroup.max.depth", "max");
  wr("a/cgroup.max.descendants", "max");
  wr("a/cgroup.max.depth", "-1");
  wr("a/cgroup.max.depth", "x");
  cat("a/cgroup.max.depth");
  wr("a/cgroup.type", "domain");
  wr("a/cgroup.type", "nope");
  wr("a/cgroup.stat", "1");
  rm("a/b/c");
  rm("a/e");
  rm("a/b");
  rm("a/nosuch");
}

static void sec_procs(void) {
  puts("== procs");
  pid_t me = getpid();
  wrn("a/cgroup.procs", me);
  self_cg("/proc/self/cgroup after move");
  cat("a/cgroup.events");
  has("a/cgroup.procs", me);
  has("cgroup.procs", me);
  has("a/cgroup.threads", tid());
  // nr_dying_descendants は消したものが片づくまで (RCU) なので見ない
  show("cgroup.stat nr_descendants", strstr(rd(path("cgroup.stat")), "nr_descendants 1\n") ? "1" : rd(path("cgroup.stat")));
  rm("a");
  // fork した子は同じ cgroup
  pid_t c = fork();
  if (c == 0) {
    self_cg("child /proc/self/cgroup");
    fflush(stdout);
    _exit(0);
  }
  reap("child", c);
  // スレッドも同じ。スレッドの番号を cgroup.procs に書くと、プロセスごと移る
  pthread_t t;
  thr_go = thr_done = 0;
  pthread_create(&t, 0, thr, 0);
  while (!__atomic_load_n(&thr_go, __ATOMIC_SEQ_CST))
    usleep(1000);
  task_cg("thread cgroup", thr_tid);
  wrn("cgroup.procs", thr_tid);
  self_cg("/proc/self/cgroup after tid move");
  task_cg("thread cgroup after tid move", thr_tid);
  // domain の cgroup の cgroup.threads はスレッドを移せない
  wrn("a/cgroup.threads", thr_tid);
  task_cg("thread cgroup after threads write", thr_tid);
  __atomic_store_n(&thr_done, 1, __ATOMIC_SEQ_CST);
  pthread_join(t, 0);
  wr("a/cgroup.procs", "0");
  self_cg("/proc/self/cgroup after 0");
  wr("a/cgroup.procs", "x");
  wr("a/cgroup.procs", "-1");
  wr("a/cgroup.procs", "999999");
  wr("a/cgroup.procs", "");
  wr("a/cgroup.procs", "1 2");
  // 移りおわった cgroup を消す
  mk("g");
  wrn("g/cgroup.procs", me);
  rm("g");
  wrn("a/cgroup.procs", me);
  rm("g");
  // 子が終わって空になる: populated
  mk("h");
  pid_t s = sleeper();
  wrn("h/cgroup.procs", s);
  cat("h/cgroup.events");
  has("h/cgroup.procs", s);
  kill(s, SIGKILL);
  reap("sleeper", s);
  cat("h/cgroup.events");
  cat("h/cgroup.procs");
  rm("h");
  // 終わった (ゾンビの) ものは移せない
  mk("z");
  s = fork();
  if (s == 0)
    _exit(3);
  usleep(100000);
  wrn("z/cgroup.procs", s);
  cat("z/cgroup.procs");
  reap("zombie", s);
  rm("z");
}

static void sec_kill(void) {
  puts("== kill");
  mk("k");
  mk("k/sub");
  pid_t s1 = sleeper(), s2 = sleeper();
  wrn("k/cgroup.procs", s1);
  wrn("k/sub/cgroup.procs", s2);
  wr("k/cgroup.kill", "2");
  wr("k/cgroup.kill", "0");
  show("cat k/cgroup.kill", rd(path("k/cgroup.kill")));
  wr("k/cgroup.kill", "1");
  reap("s1 after kill", s1);
  reap("s2 after kill", s2);
  cat("k/cgroup.events");
  rm("k/sub");
  rm("k");
  wr("cgroup.kill", "1");
}

// cgroup.events が want (frozen の 0 か 1) になるまで (2 秒まで)
static void wait_frozen(const char *rel, int want) {
  char w[64];
  snprintf(w, sizeof w, "frozen %d", want);
  for (int i = 0; i < 200 && !strstr(rd(path(rel)), w); i++)
    usleep(10000);
  char what[128];
  snprintf(what, sizeof what, "cat %s (waited)", rel);
  show(what, rd(path(rel)));
}

// /proc/PID/status の State の行
static void state(const char *what, pid_t p) {
  char f[64], l[256];
  snprintf(f, sizeof f, "/proc/%d/status", p);
  FILE *fp = fopen(f, "r");
  while (fp && fgets(l, sizeof l, fp))
    if (!strncmp(l, "State:", 6)) {
      l[strcspn(l, "\n")] = 0;
      printf("%-40s %s\n", what, l + 7);
    }
  if (fp)
    fclose(fp);
}

static void sec_freeze(void) {
  puts("== freeze");
  mk("f");
  mk("f/sub");
  pid_t s1 = sleeper(), s2 = sleeper();
  wrn("f/cgroup.procs", s1);
  wrn("f/sub/cgroup.procs", s2);
  wr("f/cgroup.freeze", "2");
  wr("f/cgroup.freeze", "x");
  wr("f/cgroup.freeze", "1");
  cat("f/cgroup.freeze");
  wait_frozen("f/cgroup.events", 1);
  wait_frozen("f/sub/cgroup.events", 1);
  cat("f/sub/cgroup.freeze");
  state("frozen sleeper State", s1);
  keys("f/cgroup.stat.local");
  // ふつうのシグナルは、とけるまで受けとらない
  kill(s1, SIGTERM);
  usleep(200000);
  printf("%-40s %s\n", "frozen s1 after SIGTERM", waitpid(s1, 0, WNOHANG) == 0 ? "alive" : "gone");
  // 凍った cgroup へ移すと凍る、出すととける
  pid_t s3 = sleeper();
  wrn("f/cgroup.procs", s3);
  usleep(100000);
  state("moved-in State", s3);
  wrn("cgroup.procs", s3);
  usleep(100000);
  cat("f/cgroup.events");
  kill(s3, SIGTERM);
  reap("moved-out s3", s3);
  // とかすと、たまっていた SIGTERM で終わる
  wr("f/cgroup.freeze", "0");
  reap("s1 after thaw", s1);
  wait_frozen("f/cgroup.events", 0);
  // SIGKILL は凍っていても効く
  wr("f/cgroup.freeze", "1");
  wait_frozen("f/sub/cgroup.events", 1);
  kill(s2, SIGKILL);
  reap("frozen s2 after SIGKILL", s2);
  cat("f/cgroup.events");
  wr("f/cgroup.freeze", "0");
  // 空の cgroup も凍る
  mk("e");
  wr("e/cgroup.freeze", "1");
  cat("e/cgroup.events");
  wr("e/cgroup.freeze", "0");
  cat("e/cgroup.events");
  rm("e");
  rm("f/sub");
  rm("f");
}

static void sec_clone3(void) {
  puts("== clone3");
  int fd = open(path("a"), O_RDONLY | O_DIRECTORY);
  struct clone_args ca = {.flags = CLONE_INTO_CGROUP, .exit_signal = SIGCHLD, .cgroup = (uint64_t)fd};
  mk("cl");
  int cfd = open(path("cl"), O_RDONLY | O_DIRECTORY);
  ca.cgroup = cfd;
  long r = syscall(SYS_clone3, &ca, sizeof ca);
  if (r == 0) {
    self_cg("clone3 child");
    fflush(stdout);
    _exit(0);
  }
  if (r < 0)
    printf("%-40s %s\n", "clone3 into cl", en(errno));
  else
    reap("clone3 child", r);
  int tfd = open("/tmp", O_RDONLY | O_DIRECTORY);
  ca.cgroup = tfd;
  r = syscall(SYS_clone3, &ca, sizeof ca);
  if (r == 0)
    _exit(0);
  printf("%-40s %s\n", "clone3 into /tmp", r < 0 ? en(errno) : "ok");
  if (r > 0)
    waitpid(r, 0, 0);
  ca.cgroup = 9999;
  r = syscall(SYS_clone3, &ca, sizeof ca);
  if (r == 0)
    _exit(0);
  printf("%-40s %s\n", "clone3 bad fd", r < 0 ? en(errno) : "ok");
  if (r > 0)
    waitpid(r, 0, 0);
  int ffd = open(path("cl/cgroup.procs"), O_RDONLY);
  ca.cgroup = ffd;
  r = syscall(SYS_clone3, &ca, sizeof ca);
  if (r == 0)
    _exit(0);
  printf("%-40s %s\n", "clone3 file fd", r < 0 ? en(errno) : "ok");
  if (r > 0)
    waitpid(r, 0, 0);
  // clone3 で作ったので struct が古い大きさ (cgroup のない 80 バイト) なら CLONE_INTO_CGROUP は EINVAL
  ca.cgroup = cfd;
  r = syscall(SYS_clone3, &ca, 80);
  if (r == 0)
    _exit(0);
  printf("%-40s %s\n", "clone3 size 80", r < 0 ? en(errno) : "ok");
  if (r > 0)
    waitpid(r, 0, 0);
  close(fd);
  close(cfd);
  close(tfd);
  close(ffd);
  rm("cl");
}

static void sec_ns(void) {
  puts("== ns");
  mk("n");
  mk("n/in");
  mk("o");
  pid_t outer = sleeper();
  wrn("o/cgroup.procs", outer);
  int pfd[2];
  pipe(pfd);
  pid_t c = fork();
  if (c == 0) {
    close(pfd[0]);
    wrn("n/cgroup.procs", getpid());
    char before[64], after[64];
    readlink("/proc/self/ns/cgroup", before, sizeof before);
    int oldns = open("/proc/self/ns/cgroup", O_RDONLY);
    printf("%-40s %s\n", "NS_GET_NSTYPE", ioctl(oldns, NS_GET_NSTYPE) == CLONE_NEWCGROUP ? "CLONE_NEWCGROUP" : en(errno));
    printf("%-40s %s\n", "unshare CLONE_NEWCGROUP", rc(unshare(CLONE_NEWCGROUP)));
    readlink("/proc/self/ns/cgroup", after, sizeof after);
    printf("%-40s %s\n", "ns/cgroup changed", strcmp(before, after) ? "yes" : "no");
    self_cg("in ns /proc/self/cgroup");
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/cgroup", getppid());
    show("in ns parent cgroup", rd(p));
    snprintf(p, sizeof p, "/proc/%d/cgroup", outer);
    show("in ns sibling cgroup", rd(p));
    // 中へ移る
    wrn("n/in/cgroup.procs", getpid());
    self_cg("in ns after move down");
    // 新しいマウントの namespace で cgroup2 をかぶせると、根は namespace の根 (n)
    printf("%-40s %s\n", "unshare CLONE_NEWNS", rc(unshare(CLONE_NEWNS)));
    mount(0, "/", 0, MS_REC | MS_PRIVATE, 0);
    mkdir("/tmp/cgn", 0755);
    printf("%-40s %s\n", "mount cgroup2 in ns", rc(mount("none", "/tmp/cgn", "cgroup2", 0, 0)));
    struct stat st;
    printf("%-40s %s\n", "ns mount has in/", stat("/tmp/cgn/in", &st) == 0 ? "yes" : en(errno));
    printf("%-40s %s\n", "ns mount has n/", stat("/tmp/cgn/n", &st) == 0 ? "yes" : en(errno));
    printf("%-40s %s\n", "ns root has cgroup.type", stat("/tmp/cgn/cgroup.type", &st) == 0 ? "yes" : en(errno));
    FILE *f = fopen("/proc/self/mountinfo", "r");
    char l[512];
    while (f && fgets(l, sizeof l, f)) {
      char *q = strstr(l, " /tmp/cgn ");
      if (!q)
        continue;
      // 根 (4 つめ) とマウント先から先
      char *a = l;
      for (int i = 0; i < 3; i++)
        a = strchr(a, ' ') + 1;
      l[strcspn(l, "\n")] = 0;
      *strchr(a, ' ') = 0;
      printf("%-40s root=%s %s\n", "ns mountinfo", a, q + 1);
    }
    if (f)
      fclose(f);
    umount("/tmp/cgn");
    rmdir("/tmp/cgn");
    // もとの namespace へ
    printf("%-40s %s\n", "setns back", rc(setns(oldns, CLONE_NEWCGROUP)));
    self_cg("after setns /proc/self/cgroup");
    printf("%-40s %s\n", "setns wrong type", rc(setns(oldns, CLONE_NEWNET)));
    fflush(stdout);
    _exit(0);
  }
  close(pfd[1]);
  reap("ns child", c);
  kill(outer, SIGKILL);
  reap("outer", outer);
  rm("n/in");
  rm("n");
  rm("o");
}

static void sec_pidfd(void) {
  puts("== pidfd");
  int pfd = syscall(SYS_pidfd_open, getpid(), 0);
  struct pidfd_info i = {.mask = PIDFD_INFO_CGROUPID};
  int r = ioctl(pfd, PIDFD_GET_INFO, &i);
  struct stat st;
  stat(path("a"), &st);
  printf("%-40s %s mask=%s eq=%s\n", "PIDFD_GET_INFO", rc(r), (i.mask & PIDFD_INFO_CGROUPID) ? "cgroupid" : "-",
         i.cgroupid == st.st_ino ? "yes" : "no");
  close(pfd);
}

static void sec_perm(void) {
  puts("== perm");
  mk("p");
  mk("p/q");
  mk("p/r");
  pid_t c = fork();
  if (c == 0) {
    if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
      _exit(9);
    self_cg("nobody /proc/self/cgroup");
    wrn("p/cgroup.procs", getpid());
    mk("p/x");
    wr("p/cgroup.max.depth", "3");
    show("nobody cat p/cgroup.max.depth", rd(path("p/cgroup.max.depth")));
    fflush(stdout);
    _exit(0);
  }
  reap("nobody child", c);
  // p を nobody に任せる (ディレクトリと cgroup.procs、cgroup.threads、subtree_control)
  const char *own[] = {"p", "p/cgroup.procs", "p/cgroup.threads", "p/cgroup.subtree_control", "p/q/cgroup.procs", "p/r/cgroup.procs"};
  for (unsigned k = 0; k < sizeof own / sizeof *own; k++)
    if (chown(path(own[k]), 65534, 65534))
      printf("%-40s %s\n", "chown", en(errno));
  struct stat st;
  stat(path("p"), &st);
  printf("%-40s uid=%u\n", "stat p after chown", st.st_uid);
  // nobody を p/q に入れておく (root が)
  int go[2];
  pipe(go);
  c = fork();
  if (c == 0) {
    close(go[1]);
    char x;
    read(go[0], &x, 1);
    if (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))
      _exit(9);
    self_cg("deleg /proc/self/cgroup");
    // p の中では動ける (共通の祖先 p の cgroup.procs が書ける)
    wrn("p/r/cgroup.procs", getpid());
    self_cg("deleg after p/q -> p/r");
    mk("p/s");
    ls("p/s");
    wrn("p/s/cgroup.procs", getpid());
    self_cg("deleg after p/r -> p/s");
    // 外 (根) へは出られない
    wrn("cgroup.procs", getpid());
    wrn("a/cgroup.procs", getpid());
    self_cg("deleg after escape tries");
    wr("p/cgroup.subtree_control", "");
    wr("p/s/cgroup.max.depth", "2");
    rm("p/s");
    fflush(stdout);
    _exit(0);
  }
  close(go[0]);
  wrn("p/q/cgroup.procs", c);
  write(go[1], "x", 1);
  close(go[1]);
  reap("deleg child", c);
  rm("p/s");
  rm("p/q");
  rm("p/r");
  rm("p");
}

int main(int argc, char **argv) {
  // console: 出力をシリアルにも (VM の中で止まったところを見る)
  if (argc > 1 && !strcmp(argv[1], "console")) {
    int c = open("/dev/console", O_WRONLY);
    if (c >= 0)
      dup2(c, 1);
  }
  setvbuf(stdout, 0, _IOLBF, 0);
  if (getuid() != 0) {
    puts("run as root (test/ldiff.sh --root cgroup)");
    return 2;
  }
  sec_proc();
  sec_mount();
  sec_mkdir();
  sec_procs();
  sec_kill();
  sec_freeze();
  sec_clone3();
  sec_ns();
  sec_pidfd();
  sec_perm();
  puts("== cleanup");
  wrn("cgroup.procs", getpid());
  rm("a");
  // 根の上限をもどす (aios の VM は同じものでテストを続ける)
  wr("cgroup.max.depth", "max");
  printf("%-40s %s\n", "umount", rc(umount(M)));
  rmdir(M);
  return bad;
}
